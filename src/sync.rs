use std::{
    collections::{hash_map::DefaultHasher, VecDeque},
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{bail, Context, Result};
use futures_util::StreamExt;
use notify::event::{EventKind, MetadataKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    sync::{mpsc, oneshot},
    time::sleep,
};
use tracing::{info, warn};

use crate::config::{ClientConfig, Config};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLocalOptions {
    pub client_id: String,
    pub local_path: PathBuf,
    pub once: bool,
    /// Retained for CLI compatibility but unused in the new pull-only design.
    pub poll: bool,
    pub server_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncTrigger {
    Startup,
    MainChanged,
    LocalFileChanged,
}

// ── Interrupt-recovery state file ────────────────────────────────────────────

/// Persisted alongside the local KDBX file as `<local_path>.sync-state.json`.
/// If `pending_promote` is `Some`, the program was interrupted after writing
/// the local file but before promoting the merge commit.  On next startup the
/// promote is retried.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SyncState {
    pending_promote: Option<PendingPromote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingPromote {
    /// Hex OID of the temporary merge commit to promote.
    commit_id: String,
    /// Hex OID of the branch tip that was current when the merge was created,
    /// or `None` if the branch did not exist yet.
    expected_branch_tip: Option<String>,
}

/// Returned when the server signals a branch conflict (409) during promote.
#[derive(Debug)]
struct BranchConflictError;

impl std::fmt::Display for BranchConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "branch was modified unexpectedly; sync-local is exiting")
    }
}
impl std::error::Error for BranchConflictError {}

// ── Public entry points ───────────────────────────────────────────────────────

pub async fn sync_local_from_config_path(
    config_path: &Path,
    options: SyncLocalOptions,
) -> Result<()> {
    let config = Config::from_file(config_path)?;
    sync_local(config, options).await
}

pub async fn sync_local(config: Config, options: SyncLocalOptions) -> Result<()> {
    run_sync_local(config, options, None).await
}

pub async fn sync_local_with_ready(
    config: Config,
    options: SyncLocalOptions,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    run_sync_local(config, options, Some(ready)).await
}

async fn run_sync_local(
    config: Config,
    options: SyncLocalOptions,
    ready: Option<oneshot::Sender<()>>,
) -> Result<()> {
    let client = config
        .clients
        .iter()
        .find(|client| client.id == options.client_id)
        .cloned()
        .ok_or_else(|| eyre::eyre!("unknown client id '{}'", options.client_id))?;

    let base_url = options
        .server_url
        .clone()
        .unwrap_or_else(|| infer_server_url(&config.bind_addr));

    let http = Client::builder()
        .build()
        .wrap_err("failed to build HTTP client")?;

    let mut syncer = RemoteSyncer::new(client, http, base_url, options);

    // On startup: handle any pending promote from a previous interrupted run,
    // then perform an initial merge-from-main.
    syncer.reconcile(SyncTrigger::Startup).await?;

    if syncer.options.once {
        if let Some(ready) = ready {
            let _ = ready.send(());
        }
        return Ok(());
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<SyncTrigger>();

    let watcher = start_local_watcher(syncer.options.local_path.clone(), tx.clone()).await?;
    let _watcher = watcher; // keep alive

    let remote_task = tokio::spawn(run_remote_event_listener(
        syncer.http.clone(),
        syncer.events_url(),
        syncer.client.username.clone(),
        syncer.client.password.clone(),
        tx.clone(),
    ));

    if fs::try_exists(&syncer.options.local_path)
        .await
        .wrap_err_with(|| {
            format!(
                "failed to stat local KDBX file {}",
                syncer.options.local_path.display()
            )
        })?
    {
        let _ = tx.send(SyncTrigger::LocalFileChanged);
    }

    if let Some(ready) = ready {
        let _ = ready.send(());
    }

    let mut pending_triggers = VecDeque::new();
    loop {
        let trigger = if let Some(trigger) = pending_triggers.pop_front() {
            trigger
        } else {
            match rx.recv().await {
                Some(trigger) => trigger,
                None => break,
            }
        };

        match trigger {
            SyncTrigger::LocalFileChanged => {
                // Wait until the local file has been idle for 400ms, resetting
                // the timer whenever a new local-file event arrives.
                let debounce_until = tokio::time::Instant::now() + Duration::from_millis(400);
                let debounce = sleep(Duration::from_millis(400));
                tokio::pin!(debounce);
                debounce.as_mut().reset(debounce_until);

                loop {
                    tokio::select! {
                        _ = &mut debounce => break,
                        received = rx.recv() => match received {
                            Some(SyncTrigger::LocalFileChanged) => {
                                debounce.as_mut().reset(
                                    tokio::time::Instant::now() + Duration::from_millis(400)
                                );
                            }
                            Some(other) => pending_triggers.push_back(other),
                            None => break,
                        }
                    }
                }

                if let Err(e) = syncer.push_local_to_webdav().await {
                    warn!(
                        "sync-local '{}': push failed: {e:#}",
                        syncer.options.client_id
                    );
                }
            }
            other => match syncer.reconcile(other).await {
                Ok(()) => {}
                Err(e) if e.downcast_ref::<BranchConflictError>().is_some() => {
                    // Fatal: branch was modified unexpectedly.
                    remote_task.abort();
                    return Err(e);
                }
                Err(e) => {
                    warn!("sync-local '{}': {e:#}", syncer.options.client_id);
                }
            },
        }
    }

    remote_task.abort();
    Ok(())
}

// ── RemoteSyncer ──────────────────────────────────────────────────────────────

struct RemoteSyncer {
    client: ClientConfig,
    http: Client,
    base_url: String,
    options: SyncLocalOptions,
    /// Path to the JSON state file used for interrupt recovery.
    state_path: PathBuf,
    /// Hash of the last content known to be in sync with the server.
    ///
    /// This is updated after successful local pushes and after pull writes
    last_synced_hash: Option<u64>,
}

impl RemoteSyncer {
    fn new(
        client: ClientConfig,
        http: Client,
        base_url: String,
        options: SyncLocalOptions,
    ) -> Self {
        let state_path = PathBuf::from(format!("{}.sync-state.json", options.local_path.display()));
        Self {
            client,
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            state_path,
            options,
            last_synced_hash: None,
        }
    }

    fn dav_url(&self) -> String {
        format!(
            "{}/dav/{}/database.kdbx",
            self.base_url, self.options.client_id
        )
    }

    fn events_url(&self) -> String {
        format!("{}/sync/{}/events", self.base_url, self.options.client_id)
    }

    fn merge_from_main_url(&self) -> String {
        format!(
            "{}/sync/{}/merge-from-main",
            self.base_url, self.options.client_id
        )
    }

    fn promote_merge_url(&self, commit_id: &str, expected_tip: &str) -> String {
        format!(
            "{}/sync/{}/promote-merge/{}?expected-tip={}",
            self.base_url, self.options.client_id, commit_id, expected_tip
        )
    }

    // ── State file helpers ────────────────────────────────────────────────────

    async fn load_state(&self) -> SyncState {
        match fs::read_to_string(&self.state_path).await {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => SyncState::default(),
        }
    }

    async fn save_state(&self, state: &SyncState) {
        let text = match serde_json::to_string(state) {
            Ok(t) => t,
            Err(e) => {
                warn!("sync-local: failed to serialise state: {e}");
                return;
            }
        };
        if let Err(e) = fs::write(&self.state_path, text).await {
            warn!("sync-local: failed to write state file: {e}");
        }
    }

    // ── Core reconciliation ───────────────────────────────────────────────────

    /// Handle a pull trigger (Startup or MainChanged).
    ///
    /// 1. If a promote is pending (interrupted run), retry it.
    /// 2. Request a new merge commit from the server.
    /// 3. If there is something to merge: write the KDBX atomically, persist
    ///    the pending promote, then promote.
    async fn reconcile(&mut self, _trigger: SyncTrigger) -> Result<()> {
        // Step 1 – recover from a previous interrupted promote.
        let state = self.load_state().await;
        if let Some(pending) = state.pending_promote {
            info!(
                "sync-local '{}': resuming interrupted promote {}",
                self.options.client_id, pending.commit_id
            );
            self.resume_pending_promote(&pending).await?;
            self.save_state(&SyncState {
                pending_promote: None,
            })
            .await;
        }

        // Step 2 – request a fresh merge from the server.
        let merge = self.request_merge_from_main().await?;

        let Some((kdbx_bytes, commit_id, expected_tip)) = merge else {
            info!(
                "sync-local '{}': already up to date with main",
                self.options.client_id
            );
            return Ok(());
        };

        info!(
            "sync-local '{}': writing merged database (commit {})",
            self.options.client_id, commit_id
        );

        // Step 3a – atomically write the local KDBX file.
        self.write_local_file_atomic(&kdbx_bytes).await?;

        // Step 3b – persist pending promote so we can recover if interrupted.
        self.save_state(&SyncState {
            pending_promote: Some(PendingPromote {
                commit_id: commit_id.clone(),
                expected_branch_tip: expected_tip.clone(),
            }),
        })
        .await;

        // Step 3c – promote the merge commit onto the client branch (retries on
        //           transient errors; fatal on 409 Conflict).
        self.do_promote(&commit_id, expected_tip.as_deref()).await?;

        self.save_state(&SyncState {
            pending_promote: None,
        })
        .await;

        info!(
            "sync-local '{}': merge promoted successfully",
            self.options.client_id
        );
        Ok(())
    }

    // ── Local push ────────────────────────────────────────────────────────────

    /// Read the local KDBX file and upload it via WebDAV PUT, unless the
    /// content matches what we last wrote ourselves (self-write suppression).
    async fn push_local_to_webdav(&mut self) -> Result<()> {
        let bytes = match fs::read(&self.options.local_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).wrap_err_with(|| {
                    format!(
                        "failed to read local KDBX file {}",
                        self.options.local_path.display()
                    )
                })
            }
        };

        // Suppress repeated local save events for content we already know is
        // in sync with the server, including our own pull writes.
        let hash = bytes_signature(&bytes);
        if Some(hash) == self.last_synced_hash {
            return Ok(());
        }

        info!(
            "sync-local '{}': pushing local change to server",
            self.options.client_id
        );

        let response = self
            .http
            .put(self.dav_url())
            .basic_auth(&self.client.username, Some(&self.client.password))
            .body(bytes)
            .send()
            .await
            .wrap_err("failed to push local KDBX to server")?;

        match response.status() {
            s if s.is_success() => {
                self.last_synced_hash = Some(hash);
                Ok(())
            }
            StatusCode::UNAUTHORIZED => bail!("server rejected sync-local credentials"),
            s => bail!("unexpected PUT status from server: {s}"),
        }
    }

    // ── Network helpers ───────────────────────────────────────────────────────

    /// Call `POST /sync/{client_id}/merge-from-main`.
    ///
    /// Returns `None` on 204 (nothing to merge), or `Some((kdbx_bytes,
    /// commit_id, expected_branch_tip))` on success.
    async fn request_merge_from_main(&self) -> Result<Option<(Vec<u8>, String, Option<String>)>> {
        let response = self
            .http
            .post(self.merge_from_main_url())
            .basic_auth(&self.client.username, Some(&self.client.password))
            .send()
            .await
            .wrap_err("failed to contact merge-from-main endpoint")?;

        match response.status() {
            StatusCode::NO_CONTENT => Ok(None),
            StatusCode::OK => {
                let commit_id = response
                    .headers()
                    .get("X-Merge-Commit-Id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();

                let expected_tip_str = response
                    .headers()
                    .get("X-Expected-Branch-Tip")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string();
                let expected_tip = if expected_tip_str == "none" {
                    None
                } else {
                    Some(expected_tip_str)
                };

                let bytes = response
                    .bytes()
                    .await
                    .wrap_err("failed to read merge-from-main response body")?
                    .to_vec();

                Ok(Some((bytes, commit_id, expected_tip)))
            }
            StatusCode::UNAUTHORIZED => bail!("server rejected sync-local credentials"),
            status => bail!("unexpected status from merge-from-main: {status}"),
        }
    }

    /// Call `POST /sync/{client_id}/promote-merge/{commit_id}`.
    ///
    /// Retries on transient errors.  Returns [`BranchConflictError`] (wrapped
    /// in [`eyre::Report`]) on 409 so the caller can exit immediately.
    async fn do_promote(&self, commit_id: &str, expected_tip: Option<&str>) -> Result<()> {
        let tip_param = expected_tip.unwrap_or("none");
        let url = self.promote_merge_url(commit_id, tip_param);

        loop {
            match self.attempt_promote(&url).await {
                Ok(()) => return Ok(()),
                Err(e) if e.downcast_ref::<BranchConflictError>().is_some() => return Err(e),
                Err(e) => {
                    warn!(
                        "sync-local '{}': promote failed (will retry): {e:#}",
                        self.options.client_id
                    );
                    sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    /// Resume a persisted pending promote from the state file.
    ///
    /// Unlike normal promote retries, recovery failures are surfaced
    /// immediately so a stale or inconsistent state file produces a useful
    /// startup error instead of retrying forever.
    async fn resume_pending_promote(&self, pending: &PendingPromote) -> Result<()> {
        let tip_param = pending.expected_branch_tip.as_deref().unwrap_or("none");
        let url = self.promote_merge_url(&pending.commit_id, tip_param);
        self.attempt_promote(&url)
            .await
            .wrap_err_with(|| format!("failed to recover pending promote {}", pending.commit_id))
    }

    async fn attempt_promote(&self, url: &str) -> Result<()> {
        let response = self
            .http
            .post(url)
            .basic_auth(&self.client.username, Some(&self.client.password))
            .send()
            .await
            .wrap_err("failed to contact promote-merge endpoint")?;

        match response.status() {
            StatusCode::OK => Ok(()),
            StatusCode::CONFLICT => Err(eyre::Report::new(BranchConflictError)),
            StatusCode::UNAUTHORIZED => bail!("server rejected sync-local credentials"),
            status => bail!("unexpected status from promote-merge: {status}"),
        }
    }

    /// Write `bytes` to the local KDBX file atomically (write to a temp file,
    /// then rename into place), and record the hash to suppress the resulting
    /// watcher event.
    async fn write_local_file_atomic(&mut self, bytes: &[u8]) -> Result<()> {
        let path = &self.options.local_path;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .await
                    .wrap_err_with(|| format!("failed to create directory {}", parent.display()))?;
            }
        }

        let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
        fs::write(&tmp_path, bytes)
            .await
            .wrap_err_with(|| format!("failed to write temp file {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .await
            .wrap_err_with(|| format!("failed to rename temp file to {}", path.display()))?;

        self.last_synced_hash = Some(bytes_signature(bytes));
        Ok(())
    }
}

// ── Local file watcher ────────────────────────────────────────────────────────

async fn start_local_watcher(
    local_path: PathBuf,
    tx: mpsc::UnboundedSender<SyncTrigger>,
) -> Result<RecommendedWatcher> {
    let watch_dir = local_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&watch_dir)
        .await
        .wrap_err_with(|| format!("failed to create watch directory {}", watch_dir.display()))?;

    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) => {
                if should_trigger_local_sync(&event) {
                    let _ = tx.send(SyncTrigger::LocalFileChanged);
                }
            }
            Err(err) => warn!("sync-local file watcher error: {err}"),
        })
        .wrap_err("failed to create local file watcher")?;

    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .wrap_err_with(|| format!("failed to watch {}", watch_dir.display()))?;
    if fs::try_exists(&local_path)
        .await
        .wrap_err_with(|| format!("failed to stat {}", local_path.display()))?
    {
        watcher
            .watch(&local_path, RecursiveMode::NonRecursive)
            .wrap_err_with(|| format!("failed to watch {}", local_path.display()))?;
    }

    Ok(watcher)
}

fn should_trigger_local_sync(event: &notify::Event) -> bool {
    !matches!(
        event.kind,
        EventKind::Access(_)
            | EventKind::Other
            | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
}

// ── SSE event listener ────────────────────────────────────────────────────────

async fn run_remote_event_listener(
    http: Client,
    events_url: String,
    username: String,
    password: String,
    tx: mpsc::UnboundedSender<SyncTrigger>,
) {
    loop {
        let request = http
            .get(&events_url)
            .basic_auth(&username, Some(&password))
            .header(header::ACCEPT, "text/event-stream");

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let mut stream = response.bytes_stream();
                let mut buffer = String::new();

                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));
                            buffer = buffer.replace("\r\n", "\n");
                            drain_sse_frames(&mut buffer, &tx);
                        }
                        Err(err) => {
                            warn!("sync-local remote event stream error: {err}");
                            break;
                        }
                    }
                }
            }
            Ok(response) => warn!(
                "sync-local remote event stream returned {}",
                response.status()
            ),
            Err(err) => warn!("sync-local failed to connect to event stream: {err}"),
        }

        if tx.is_closed() {
            return;
        }

        sleep(Duration::from_secs(1)).await;
    }
}

fn drain_sse_frames(buffer: &mut String, tx: &mpsc::UnboundedSender<SyncTrigger>) {
    while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        *buffer = buffer[idx + 2..].to_string();

        let mut event_name = None;
        for line in frame.lines() {
            if let Some(name) = line.strip_prefix("event:") {
                event_name = Some(name.trim().to_string());
            }
        }

        if event_name.as_deref() == Some("branch-updated") {
            let _ = tx.send(SyncTrigger::MainChanged);
        }
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn bytes_signature(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn infer_server_url(bind_addr: &str) -> String {
    if bind_addr.contains("://") {
        return bind_addr.trim_end_matches('/').to_string();
    }

    if let Ok(addr) = bind_addr.parse::<SocketAddr>() {
        let host = match addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            other => other,
        };
        return format!("http://{}:{}", host, addr.port());
    }

    format!("http://{}", bind_addr.trim_end_matches('/'))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    #[test]
    fn infer_server_url_uses_loopback_for_wildcard_bind() {
        assert_eq!(infer_server_url("0.0.0.0:8080"), "http://127.0.0.1:8080");
    }

    #[test]
    fn infer_server_url_keeps_explicit_urls() {
        assert_eq!(
            infer_server_url("https://example.com/base/"),
            "https://example.com/base"
        );
    }

    #[tokio::test]
    async fn remote_listener_parses_main_branch_updates() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut buffer =
            String::from("event: ready\ndata: 0\n\nevent: branch-updated\ndata: 1\n\n");

        drain_sse_frames(&mut buffer, &tx);

        assert_eq!(
            timeout(Duration::from_millis(50), rx.recv()).await.unwrap(),
            Some(SyncTrigger::MainChanged)
        );
    }

    #[tokio::test]
    async fn local_watcher_ignores_read_only_file_access() {
        let tempdir = TempDir::new().unwrap();
        let local_path = tempdir.path().join("alice.kdbx");
        fs::write(&local_path, b"seed").await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = start_local_watcher(local_path.clone(), tx).await.unwrap();

        // Drain any startup events.
        while timeout(Duration::from_millis(100), rx.recv()).await.is_ok() {}

        let _ = fs::read(&local_path).await.unwrap();

        assert!(
            timeout(Duration::from_millis(500), rx.recv())
                .await
                .is_err(),
            "read-only access should not trigger a local sync"
        );
    }
}
