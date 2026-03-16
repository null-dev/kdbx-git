use std::{
    collections::hash_map::DefaultHasher,
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
use tokio::{
    fs,
    sync::{mpsc, oneshot},
    task::spawn_blocking,
    time::sleep,
};
use tracing::{info, warn};

use crate::{
    config::{ClientConfig, Config},
    kdbx::{build_kdbx_sync, parse_kdbx_sync},
    storage::{
        format::{serialize, StorageFormat},
        types::StorageDatabase,
    },
    store::merge_databases,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLocalOptions {
    pub client_id: String,
    pub local_path: PathBuf,
    pub once: bool,
    pub poll: bool,
    pub server_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncTrigger {
    Startup,
    LocalChange,
    RemoteChange,
}

#[derive(Debug, Clone)]
struct SyncSnapshot {
    remote_storage: Option<StorageDatabase>,
    remote_sig: Option<u64>,
    local_storage: Option<StorageDatabase>,
    local_sig: Option<u64>,
}

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

    let mut syncer = RemoteSyncer::new(config, client, http, base_url, options);
    syncer.reconcile(SyncTrigger::Startup).await?;

    if syncer.options.once {
        if let Some(ready) = ready {
            let _ = ready.send(());
        }
        return Ok(());
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let watcher = start_local_watcher(syncer.options.local_path.clone(), tx.clone()).await?;
    let remote_task = tokio::spawn(run_remote_event_listener(
        syncer.http.clone(),
        syncer.events_url(),
        syncer.client.username.clone(),
        syncer.client.password.clone(),
        tx.clone(),
    ));
    let local_probe = syncer.options.poll.then(|| {
        tokio::spawn(run_local_change_probe(
            syncer.options.local_path.clone(),
            tx.clone(),
        ))
    });

    let _watcher = watcher;
    if let Some(ready) = ready {
        let _ = ready.send(());
    }

    while let Some(trigger) = rx.recv().await {
        if trigger == SyncTrigger::LocalChange {
            // Let the writer finish flushing the local file before we parse it.
            sleep(Duration::from_millis(400)).await;
        }

        if let Err(err) = syncer.reconcile(trigger).await {
            warn!("sync-local '{}': {err:#}", syncer.options.client_id);
            if trigger == SyncTrigger::LocalChange {
                let retry_tx = tx.clone();
                tokio::spawn(async move {
                    sleep(Duration::from_millis(300)).await;
                    let _ = retry_tx.send(SyncTrigger::LocalChange);
                });
            }
        }
    }

    remote_task.abort();
    if let Some(local_probe) = local_probe {
        local_probe.abort();
    }
    Ok(())
}

struct RemoteSyncer {
    config: Config,
    client: ClientConfig,
    http: Client,
    base_url: String,
    options: SyncLocalOptions,
    last_remote_sig: Option<u64>,
    last_local_sig: Option<u64>,
}

impl RemoteSyncer {
    fn new(
        config: Config,
        client: ClientConfig,
        http: Client,
        base_url: String,
        options: SyncLocalOptions,
    ) -> Self {
        Self {
            config,
            client,
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            options,
            last_remote_sig: None,
            last_local_sig: None,
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

    async fn reconcile(&mut self, trigger: SyncTrigger) -> Result<()> {
        let snapshot = self.read_snapshot().await?;

        match (
            snapshot.remote_storage.as_ref(),
            snapshot.local_storage.as_ref(),
        ) {
            (None, None) => {
                info!(
                    "sync-local '{}': both branch and local file are empty",
                    self.options.client_id
                );
            }
            (Some(remote), None) => {
                info!(
                    "sync-local '{}': writing server state to {}",
                    self.options.client_id,
                    self.options.local_path.display()
                );
                self.write_local_file(remote).await?;
            }
            (None, Some(local)) => {
                info!(
                    "sync-local '{}': uploading local file to server",
                    self.options.client_id
                );
                self.push_remote(local.clone()).await?;
            }
            (Some(remote), Some(local)) => {
                if snapshot.remote_sig == snapshot.local_sig {
                    info!(
                        "sync-local '{}': local file and server already match",
                        self.options.client_id
                    );
                } else {
                    let remote_changed = snapshot.remote_sig != self.last_remote_sig;
                    let local_changed = snapshot.local_sig != self.last_local_sig;

                    match trigger {
                        SyncTrigger::RemoteChange if !local_changed => {
                            info!(
                                "sync-local '{}': applying remote change to local file",
                                self.options.client_id
                            );
                            self.write_local_file(remote).await?;
                        }
                        SyncTrigger::LocalChange if !remote_changed => {
                            info!(
                                "sync-local '{}': uploading local change to server",
                                self.options.client_id
                            );
                            self.push_remote(local.clone()).await?;
                        }
                        _ if remote_changed && !local_changed => {
                            info!(
                                "sync-local '{}': applying remote change to local file",
                                self.options.client_id
                            );
                            self.write_local_file(remote).await?;
                        }
                        _ if local_changed && !remote_changed => {
                            info!(
                                "sync-local '{}': uploading local change to server",
                                self.options.client_id
                            );
                            self.push_remote(local.clone()).await?;
                        }
                        _ => {
                            warn!(
                                "sync-local '{}': local file and server diverged, merging both",
                                self.options.client_id
                            );
                            let merged = merge_databases(remote, local)?;
                            self.push_remote(merged.clone()).await?;
                            self.write_local_file(&merged).await?;
                        }
                    }
                }
            }
        }

        let refreshed = self.read_snapshot().await?;
        self.last_remote_sig = refreshed.remote_sig;
        self.last_local_sig = refreshed.local_sig;
        Ok(())
    }

    async fn read_snapshot(&self) -> Result<SyncSnapshot> {
        let remote_storage = self.fetch_remote_storage().await?;
        let remote_sig = remote_storage
            .as_ref()
            .map(stable_storage_sig)
            .transpose()?;

        let local_storage = self.read_local_storage().await?;
        let local_sig = local_storage.as_ref().map(stable_storage_sig).transpose()?;

        Ok(SyncSnapshot {
            remote_storage,
            remote_sig,
            local_storage,
            local_sig,
        })
    }

    async fn fetch_remote_storage(&self) -> Result<Option<StorageDatabase>> {
        let response = self
            .http
            .get(self.dav_url())
            .basic_auth(&self.client.username, Some(&self.client.password))
            .send()
            .await
            .wrap_err("failed to fetch remote KDBX over HTTP")?;

        match response.status() {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .wrap_err("failed to read remote KDBX response body")?;
                let creds = self.config.database.clone();
                let storage = spawn_blocking(move || parse_kdbx_sync(&bytes, &creds))
                    .await
                    .wrap_err("remote KDBX parse task panicked")??;
                Ok(Some(storage))
            }
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::UNAUTHORIZED => bail!("server rejected sync-local credentials"),
            status => bail!("unexpected GET status from server: {status}"),
        }
    }

    async fn read_local_storage(&self) -> Result<Option<StorageDatabase>> {
        let bytes = match fs::read(&self.options.local_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).wrap_err_with(|| {
                    format!(
                        "failed to read local KDBX file {}",
                        self.options.local_path.display()
                    )
                })
            }
        };

        let creds = self.config.database.clone();
        let path = self.options.local_path.clone();
        let storage = spawn_blocking(move || parse_kdbx_sync(&bytes, &creds))
            .await
            .wrap_err("local KDBX parse task panicked")?
            .wrap_err_with(|| format!("failed to parse local KDBX file {}", path.display()))?;

        Ok(Some(storage))
    }

    async fn write_local_file(&self, storage: &StorageDatabase) -> Result<()> {
        if let Some(parent) = self.options.local_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .await
                    .wrap_err_with(|| format!("failed to create directory {}", parent.display()))?;
            }
        }

        let creds = self.config.database.clone();
        let storage = storage.clone();
        let path = self.options.local_path.clone();
        let bytes = spawn_blocking(move || build_kdbx_sync(&storage, &creds))
            .await
            .wrap_err("local KDBX build task panicked")??;

        fs::write(&path, bytes)
            .await
            .wrap_err_with(|| format!("failed to write local KDBX file {}", path.display()))?;
        Ok(())
    }

    async fn push_remote(&self, storage: StorageDatabase) -> Result<()> {
        let creds = self.config.database.clone();
        let bytes = spawn_blocking(move || build_kdbx_sync(&storage, &creds))
            .await
            .wrap_err("remote KDBX build task panicked")??;

        let response = self
            .http
            .put(self.dav_url())
            .basic_auth(&self.client.username, Some(&self.client.password))
            .body(bytes)
            .send()
            .await
            .wrap_err("failed to upload local KDBX over HTTP")?;

        match response.status() {
            status if status.is_success() => Ok(()),
            StatusCode::UNAUTHORIZED => bail!("server rejected sync-local credentials"),
            status => bail!("unexpected PUT status from server: {status}"),
        }
    }
}

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
                    let _ = tx.send(SyncTrigger::LocalChange);
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

async fn run_local_change_probe(local_path: PathBuf, tx: mpsc::UnboundedSender<SyncTrigger>) {
    let mut last_seen = read_local_content_fingerprint(&local_path)
        .await
        .ok()
        .flatten();

    loop {
        sleep(Duration::from_millis(250)).await;

        let current = match read_local_content_fingerprint(&local_path).await {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    "sync-local local probe failed to stat {}: {err}",
                    local_path.display()
                );
                continue;
            }
        };

        if current != last_seen {
            last_seen = current;
            let _ = tx.send(SyncTrigger::LocalChange);
        }

        if tx.is_closed() {
            return;
        }
    }
}

async fn read_local_content_fingerprint(local_path: &Path) -> Result<Option<u64>> {
    match fs::read(local_path).await {
        Ok(bytes) => Ok(Some(bytes_signature(&bytes))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err_with(|| format!("failed to read {}", local_path.display())),
    }
}

fn should_trigger_local_sync(event: &notify::Event) -> bool {
    !matches!(
        event.kind,
        EventKind::Access(_)
            | EventKind::Other
            | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
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
            let _ = tx.send(SyncTrigger::RemoteChange);
        }
    }
}

fn stable_storage_sig(storage: &StorageDatabase) -> Result<u64> {
    let repr = serialize(storage, StorageFormat::Json)?;
    Ok(bytes_signature(repr.as_bytes()))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
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
    async fn remote_listener_parses_branch_updates() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut buffer =
            String::from("event: ready\ndata: 0\n\nevent: branch-updated\ndata: 1\n\n");

        drain_sse_frames(&mut buffer, &tx);

        assert_eq!(
            timeout(Duration::from_millis(50), rx.recv()).await.unwrap(),
            Some(SyncTrigger::RemoteChange)
        );
    }

    #[tokio::test]
    async fn local_watcher_ignores_read_only_file_access() {
        let tempdir = TempDir::new().unwrap();
        let local_path = tempdir.path().join("alice.kdbx");
        fs::write(&local_path, b"seed").await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = start_local_watcher(local_path.clone(), tx).await.unwrap();

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
