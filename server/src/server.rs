//! Steps 7 & 8 — WebDAV server, HTTP Basic Auth, and concurrency control.
//!
//! # Architecture
//!
//! Each request is handled like this:
//!
//! 1. **Auth middleware** extracts the client ID from the URL path
//!    (`/dav/{client_id}/...`), validates Basic Auth credentials against the
//!    config, and stores the validated `client_id` in request extensions.
//!
//! 2. **`dav_handler`** retrieves the `client_id` from extensions, builds a
//!    per-request [`KdbxFs`] and a [`DavHandler`] with `strip_prefix` set to
//!    `/dav/{client_id}`, and delegates to dav-server.
//!
//! 3. **[`KdbxFs`]** implements [`DavFileSystem`] against a single virtual
//!    file `/database.kdbx`:
//!    - `metadata("/")` → root collection
//!    - `metadata("/database.kdbx")` → file exists iff the branch has commits
//!    - `open(read)` → merge main into the client branch, then build KDBX bytes
//!    - `open(write)` → accumulate bytes; on `flush`, decrypt and write to git
//!
//! 4. **[`AppState`]** wraps [`GitStore`] in `Arc<tokio::sync::Mutex<...>>`
//!    (step 8) so concurrent writes are serialised.

use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    io::SeekFrom,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    extract::{Json, Path, Query, Request, State},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{any, get, post},
    Extension, Router,
};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine};
use bytes::Bytes;
use chrono::Utc;
use dav_server::{
    fakels::FakeLs,
    fs::{
        DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
        OpenOptions, ReadDirMeta,
    },
    DavHandler,
};
use eyre::{Context, Result};
use futures_util::{stream, StreamExt};
use gix::ObjectId;
use http::{header, StatusCode};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{watch, Mutex},
    task::spawn_blocking,
};
use tracing::{info, warn};

use crate::{
    config::Config,
    kdbx::{build_kdbx_sync, parse_kdbx_sync},
    store::{BranchConflictError, GitStore, MAIN_BRANCH},
    sync_state::{RevokedPushEndpoint, SyncStateStore},
};

const PUSH_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

type PushDeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<StatusCode, PushDeliveryError>> + Send + 'a>>;

#[derive(Debug)]
struct PushDeliveryError(String);

impl std::fmt::Display for PushDeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PushDeliveryError {}

trait PushDelivery: Send + Sync {
    fn post_branch_updated<'a>(&'a self, endpoint: &'a str) -> PushDeliveryFuture<'a>;
}

#[derive(Clone)]
struct ReqwestPushDelivery {
    client: reqwest::Client,
}

#[derive(Serialize)]
struct BranchUpdatedNotification<'a> {
    event: &'a str,
}

impl PushDelivery for ReqwestPushDelivery {
    fn post_branch_updated<'a>(&'a self, endpoint: &'a str) -> PushDeliveryFuture<'a> {
        Box::pin(async move {
            self.client
                .post(endpoint)
                .json(&BranchUpdatedNotification {
                    event: "branch-updated",
                })
                .send()
                .await
                .map(|response| response.status())
                .map_err(|err| PushDeliveryError(err.to_string()))
        })
    }
}

// ── AppState (step 8) ─────────────────────────────────────────────────────────

/// Shared server state. All fields behind cheap-to-clone handles.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<GitStore>>,
    pub config: Arc<Config>,
    push_state: Arc<Mutex<SyncStateStore>>,
    push_delivery: Arc<dyn PushDelivery>,
    /// Per-branch notification channels.  Includes an entry for `MAIN_BRANCH`
    /// so sync-local clients can be notified when main advances.
    branch_notifications: Arc<HashMap<String, watch::Sender<u64>>>,
}

impl AppState {
    pub fn new(config: Config, store: GitStore) -> Self {
        let push_delivery = ReqwestPushDelivery {
            client: reqwest::Client::builder()
                .timeout(PUSH_DELIVERY_TIMEOUT)
                .build()
                .expect("failed to build push delivery HTTP client"),
        };
        Self::new_with_push_delivery(config, store, Arc::new(push_delivery))
    }

    fn new_with_push_delivery(
        config: Config,
        store: GitStore,
        push_delivery: Arc<dyn PushDelivery>,
    ) -> Self {
        let mut branch_notifications: HashMap<String, watch::Sender<u64>> = config
            .clients
            .iter()
            .map(|client| {
                let (tx, _rx) = watch::channel(0_u64);
                (client.id.clone(), tx)
            })
            .collect();

        // Add a channel for the main branch so sync-local clients are notified
        // when main advances after any client write.
        let (main_tx, _) = watch::channel(0_u64);
        branch_notifications.insert(MAIN_BRANCH.to_string(), main_tx);

        Self {
            store: Arc::new(Mutex::new(store)),
            push_state: Arc::new(Mutex::new(SyncStateStore::for_git_store(&config.git_store))),
            push_delivery,
            config: Arc::new(config),
            branch_notifications: Arc::new(branch_notifications),
        }
    }

    fn subscribe_branch_notifications(&self, branch_id: &str) -> Option<watch::Receiver<u64>> {
        self.branch_notifications
            .get(branch_id)
            .map(watch::Sender::subscribe)
    }

    fn notify_branches<'a>(&self, branch_ids: impl IntoIterator<Item = &'a String>) {
        for branch_id in branch_ids {
            if let Some(tx) = self.branch_notifications.get(branch_id) {
                tx.send_modify(|version| *version += 1);
            }
        }
    }

    async fn upsert_push_endpoint(&self, client_id: &str, endpoint: String) -> Result<()> {
        let store = self.push_state.lock().await;
        store.upsert_push_endpoint(client_id, endpoint, Utc::now())
    }

    async fn remove_push_endpoint(&self, client_id: &str) -> Result<()> {
        let store = self.push_state.lock().await;
        store.remove_push_endpoint(client_id, Utc::now())
    }

    async fn load_push_delivery_targets(&self) -> Result<Vec<(String, String)>> {
        let store = self.push_state.lock().await;
        let state = store.load()?;
        Ok(state
            .push_endpoints
            .into_iter()
            .map(|(client_id, entry)| (client_id, entry.endpoint))
            .collect())
    }

    async fn remove_revoked_push_endpoints(&self, revoked: &[RevokedPushEndpoint]) -> Result<()> {
        let store = self.push_state.lock().await;
        store.remove_revoked_push_endpoints(revoked, Utc::now())
    }

    async fn dispatch_push_notifications(&self) -> Result<()> {
        let targets = self.load_push_delivery_targets().await?;
        if targets.is_empty() {
            return Ok(());
        }

        let mut revoked = Vec::new();
        for (client_id, endpoint) in targets {
            match self.push_delivery.post_branch_updated(&endpoint).await {
                Ok(StatusCode::NOT_FOUND | StatusCode::GONE) => revoked.push(RevokedPushEndpoint {
                    client_id,
                    endpoint,
                }),
                Ok(status) if status.is_success() => {}
                Ok(status) => {
                    warn!(
                        "push delivery to '{}' for client '{}' returned {}",
                        endpoint, client_id, status
                    );
                }
                Err(err) => {
                    warn!(
                        "push delivery to '{}' for client '{}' failed: {}",
                        endpoint, client_id, err
                    );
                }
            }
        }

        if !revoked.is_empty() {
            self.remove_revoked_push_endpoints(&revoked).await?;
        }

        Ok(())
    }
}

// ── Metadata types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FileMeta {
    len: u64,
    modified: SystemTime,
}

#[derive(Debug, Clone)]
struct DirMeta;

impl DavMetaData for FileMeta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }
    fn is_dir(&self) -> bool {
        false
    }
}

impl DavMetaData for DirMeta {
    fn len(&self) -> u64 {
        0
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(SystemTime::now())
    }
    fn is_dir(&self) -> bool {
        true
    }
}

// ── Directory entry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DbFileEntry;

impl DavDirEntry for DbFileEntry {
    fn name(&self) -> Vec<u8> {
        b"database.kdbx".to_vec()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(futures_util::future::ready(Ok(Box::new(FileMeta {
            len: 0,
            modified: SystemTime::now(),
        })
            as Box<dyn DavMetaData>)))
    }
}

// ── DavFile implementations ───────────────────────────────────────────────────

/// Readable file backed by in-memory KDBX bytes.
#[derive(Debug)]
struct ReadFile {
    data: Bytes,
    pos: usize,
}

impl DavFile for ReadFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let len = self.data.len() as u64;
        Box::pin(futures_util::future::ready(Ok(Box::new(FileMeta {
            len,
            modified: SystemTime::now(),
        })
            as Box<dyn DavMetaData>)))
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(futures_util::future::ready(Err(FsError::Forbidden)))
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(futures_util::future::ready(Err(FsError::Forbidden)))
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        let start = self.pos;
        let end = (self.pos + count).min(self.data.len());
        let slice = self.data.slice(start..end);
        self.pos = end;
        Box::pin(futures_util::future::ready(Ok(slice)))
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        let len = self.data.len() as u64;
        let new_pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => (len as i64 + n).max(0) as u64,
            SeekFrom::Current(n) => (self.pos as i64 + n).max(0) as u64,
        };
        self.pos = (new_pos as usize).min(self.data.len());
        Box::pin(futures_util::future::ready(Ok(new_pos)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(futures_util::future::ready(Ok(())))
    }
}

/// Writable buffer that decrypts and commits to git on `flush`.
#[derive(Debug)]
struct WriteFile {
    buf: Vec<u8>,
    state: AppState,
    client_id: String,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

impl DavFile for WriteFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let len = self.buf.len() as u64;
        Box::pin(futures_util::future::ready(Ok(Box::new(FileMeta {
            len,
            modified: SystemTime::now(),
        })
            as Box<dyn DavMetaData>)))
    }

    fn write_buf(&mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        use bytes::Buf;
        while buf.has_remaining() {
            let chunk = buf.chunk();
            let len = chunk.len();
            self.buf.extend_from_slice(chunk);
            buf.advance(len);
        }
        Box::pin(futures_util::future::ready(Ok(())))
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        self.buf.extend_from_slice(&buf);
        Box::pin(futures_util::future::ready(Ok(())))
    }

    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(futures_util::future::ready(Err(FsError::Forbidden)))
    }

    fn seek(&mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(futures_util::future::ready(Err(FsError::NotImplemented)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        let bytes = self.buf.clone();
        let state = self.state.clone();
        let client_id = self.client_id.clone();

        Box::pin(async move {
            let config = Arc::clone(&state.config);

            // Parse KDBX bytes (blocking crypto work)
            let storage = spawn_blocking(move || parse_kdbx_sync(&bytes, &config.database))
                .await
                .map_err(|_| FsError::GeneralFailure)?
                .map_err(|e| {
                    warn!("Client '{}': failed to parse KDBX: {e:#}", client_id);
                    FsError::Forbidden
                })?;

            // Commit to git (serialised by the mutex)
            state
                .store
                .lock()
                .await
                .process_client_write(client_id.clone(), storage)
                .await
                .map(|updated_branches| {
                    let main_advanced = updated_branches.iter().any(|branch| branch == MAIN_BRANCH);
                    state.notify_branches(updated_branches.iter());
                    if main_advanced {
                        let push_state = state.clone();
                        tokio::spawn(async move {
                            if let Err(err) = push_state.dispatch_push_notifications().await {
                                warn!("push delivery task failed: {err:#}");
                            }
                        });
                    }
                })
                .map_err(|e| {
                    warn!("Client '{}': git write failed: {e:#}", client_id);
                    FsError::GeneralFailure
                })?;

            info!("Client '{}' write committed", client_id);
            Ok(())
        })
    }
}

// ── KdbxFs ────────────────────────────────────────────────────────────────────

const DB_FILE: &str = "database.kdbx";

/// Per-request DavFileSystem. Exposes a single virtual file `/database.kdbx`.
#[derive(Clone)]
struct KdbxFs {
    state: AppState,
    client_id: String,
}

impl KdbxFs {
    fn new(state: AppState, client_id: String) -> Box<Self> {
        Box::new(Self { state, client_id })
    }
}

impl DavFileSystem for KdbxFs {
    fn open<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        let path_str = path.as_url_string();
        let state = self.state.clone();
        let client_id = self.client_id.clone();

        Box::pin(async move {
            if path_str.trim_matches('/') != DB_FILE {
                return Err(FsError::NotFound);
            }

            if options.write || options.create || options.create_new {
                Ok(Box::new(WriteFile {
                    buf: Vec::new(),
                    state,
                    client_id,
                }) as Box<dyn DavFile>)
            } else {
                // Merge main into the client branch first, so the client always
                // sees the latest merged state.  Failure is non-fatal.
                {
                    let store = state.store.lock().await;
                    match store.merge_main_into_branch(client_id.clone()).await {
                        Ok(true) => {
                            // Notify main-branch subscribers (no-op if none).
                            state.notify_branches([&MAIN_BRANCH.to_string()]);
                        }
                        Ok(false) => {} // already up to date
                        Err(e) => {
                            warn!(
                                "Client '{}': failed to merge main on read (serving stale data): {e:#}",
                                client_id
                            );
                        }
                    }
                }

                // Generate KDBX bytes from the branch tip.
                let config = Arc::clone(&state.config);
                let storage = {
                    let store = state.store.lock().await;
                    store
                        .read_branch(client_id.clone())
                        .await
                        .map_err(|e| {
                            warn!("Client '{}': failed to read branch: {e:#}", client_id);
                            FsError::GeneralFailure
                        })?
                        .ok_or(FsError::NotFound)?
                };

                let bytes = spawn_blocking(move || build_kdbx_sync(&storage, &config.database))
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .map_err(|e| {
                        warn!("Client '{}': failed to build KDBX: {e:#}", client_id);
                        FsError::GeneralFailure
                    })?;

                Ok(Box::new(ReadFile {
                    data: Bytes::from(bytes),
                    pos: 0,
                }) as Box<dyn DavFile>)
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        let path_str = path.as_url_string();
        Box::pin(async move {
            if path_str.trim_matches('/').is_empty() {
                let entry: Box<dyn DavDirEntry> = Box::new(DbFileEntry);
                let s = stream::once(futures_util::future::ready(Ok(entry)));
                Ok(Box::pin(s) as FsStream<Box<dyn DavDirEntry>>)
            } else {
                Err(FsError::NotFound)
            }
        })
    }

    fn metadata<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        let path_str = path.as_url_string();
        let state = self.state.clone();
        let client_id = self.client_id.clone();

        Box::pin(async move {
            let trimmed = path_str.trim_matches('/');

            if trimmed.is_empty() {
                return Ok(Box::new(DirMeta) as Box<dyn DavMetaData>);
            }

            if trimmed == DB_FILE {
                let exists = state
                    .store
                    .lock()
                    .await
                    .branch_tip_id(client_id.clone())
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .is_some();

                if exists {
                    Ok(Box::new(FileMeta {
                        len: 0,
                        modified: SystemTime::now(),
                    }) as Box<dyn DavMetaData>)
                } else {
                    Err(FsError::NotFound)
                }
            } else {
                Err(FsError::NotFound)
            }
        })
    }
}

// ── HTTP Basic Auth middleware ─────────────────────────────────────────────────

/// Validated client ID, injected into request extensions by the auth middleware.
#[derive(Clone)]
struct AuthedClientId(String);

async fn auth_middleware(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let unauthorized = || -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"kdbx-git\"")],
        )
            .into_response()
    };

    // Extract client_id from path: /dav/{client_id}/... or /sync/{client_id}/...
    let path = req.uri().path().to_owned();
    let client_id = extract_client_id_from_path(&path);

    let client_id = match client_id {
        Some(id) => id,
        None => return unauthorized(),
    };

    // Decode Basic Auth credentials
    let creds = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| B64_STANDARD.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());

    let (username, password) = match creds.as_deref().and_then(|s| s.split_once(':')) {
        Some(pair) => pair,
        None => return unauthorized(),
    };

    // Match credentials against the client in config
    let found = state
        .config
        .clients
        .iter()
        .any(|c| c.id == client_id && c.id == username && c.password == password);

    if found {
        req.extensions_mut().insert(AuthedClientId(client_id));
        next.run(req).await
    } else {
        unauthorized()
    }
}

fn extract_client_id_from_path(path: &str) -> Option<String> {
    ["/dav/", "/sync/", "/push/"]
        .into_iter()
        .find_map(|prefix| {
            path.strip_prefix(prefix)
                .and_then(|s| s.split('/').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
}

// ── WebDAV request handler ─────────────────────────────────────────────────────

async fn dav_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let client_id = match req.extensions().get::<AuthedClientId>() {
        Some(id) => id.0.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    // Ensure the branch exists before the request is processed
    {
        let store = state.store.lock().await;
        if let Err(e) = store.ensure_client_branch(client_id.clone()).await {
            warn!("Failed to ensure branch for '{}': {e:#}", client_id);
        }
    }

    let prefix = format!("/dav/{client_id}");
    let fs = KdbxFs::new(state, client_id);
    let dav = DavHandler::builder()
        .filesystem(fs)
        .locksystem(FakeLs::new())
        .autoindex(true)
        .strip_prefix(prefix)
        .build_handler();

    dav.handle(req).await.into_response()
}

// ── Sync-local event stream ────────────────────────────────────────────────────

/// SSE stream that fires whenever `main` advances.  Used by sync-local clients
/// to know when to pull a new merge from the server.
async fn sync_events_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let client_id = match req.extensions().get::<AuthedClientId>() {
        Some(id) => id.0.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    {
        let store = state.store.lock().await;
        if let Err(e) = store.ensure_client_branch(client_id.clone()).await {
            warn!("Failed to ensure branch for '{}': {e:#}", client_id);
        }
    }

    // Subscribe to main-branch notifications.
    let Some(receiver) = state.subscribe_branch_notifications(MAIN_BRANCH) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let initial =
        stream::once(async { Ok::<Event, Infallible>(Event::default().event("ready").data("0")) });
    let updates = stream::unfold(receiver, |mut receiver| async move {
        match receiver.changed().await {
            Ok(()) => {
                let version = *receiver.borrow_and_update();
                Some((
                    Ok::<Event, Infallible>(
                        Event::default()
                            .event("branch-updated")
                            .data(version.to_string()),
                    ),
                    receiver,
                ))
            }
            Err(_) => None,
        }
    });

    Sse::new(initial.chain(updates))
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ── Sync-local merge endpoints ────────────────────────────────────────────────

/// `POST /sync/{client_id}/merge-from-main`
///
/// Creates a temporary merge commit that merges `main` into the client's branch
/// and returns the resulting KDBX bytes.
///
/// Response headers:
/// - `X-Merge-Commit-Id`: hex OID of the temporary commit
/// - `X-Expected-Branch-Tip`: hex OID of the client branch tip at merge time,
///   or `"none"` if the branch did not exist
///
/// Returns **204 No Content** when there is nothing to merge (the client branch
/// already contains `main`).
async fn sync_merge_from_main_handler(
    State(state): State<AppState>,
    req: Request,
) -> impl IntoResponse {
    let client_id = match req.extensions().get::<AuthedClientId>() {
        Some(id) => id.0.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let result = {
        let store = state.store.lock().await;
        store.create_sync_merge_commit(client_id.clone()).await
    };

    match result {
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(merge_result)) => {
            let config = Arc::clone(&state.config);
            let commit_id = merge_result.commit_id.to_hex().to_string();
            let expected_tip = match merge_result.expected_branch_tip {
                Some(id) => id.to_hex().to_string(),
                None => "none".to_string(),
            };
            let storage = merge_result.storage;

            match spawn_blocking(move || build_kdbx_sync(&storage, &config.database)).await {
                Ok(Ok(bytes)) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header("X-Merge-Commit-Id", &commit_id)
                    .header("X-Expected-Branch-Tip", &expected_tip)
                    .body(axum::body::Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                Ok(Err(e)) => {
                    warn!(
                        "sync merge-from-main: failed to build KDBX for '{}': {e:#}",
                        client_id
                    );
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Err(e) => {
            warn!("sync merge-from-main: failed for '{}': {e:#}", client_id);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Path params for the promote-merge route.
#[derive(Deserialize)]
struct PromoteMergePathParams {
    #[allow(dead_code)]
    client_id: String,
    commit_id: String,
}

/// Query params for the promote-merge route.
#[derive(Deserialize)]
struct PromoteMergeQuery {
    /// Hex OID of the branch tip that was current when the merge was created,
    /// or `"none"` if the branch did not exist.
    #[serde(rename = "expected-tip")]
    expected_tip: String,
}

/// `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip=<hex|none>`
///
/// Promotes the temporary merge commit created by `merge-from-main` onto the
/// client's branch.  The `expected-tip` query parameter must match the branch
/// tip that was current when `merge-from-main` was called.
///
/// Returns **409 Conflict** if the branch was modified unexpectedly.
async fn sync_promote_merge_handler(
    State(state): State<AppState>,
    Path(PromoteMergePathParams {
        commit_id: commit_id_str,
        ..
    }): Path<PromoteMergePathParams>,
    Query(query): Query<PromoteMergeQuery>,
    req: Request,
) -> impl IntoResponse {
    let client_id = match req.extensions().get::<AuthedClientId>() {
        Some(id) => id.0.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let commit_id = match ObjectId::from_hex(commit_id_str.as_bytes()) {
        Ok(id) => id,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let expected_branch_tip: Option<ObjectId> = if query.expected_tip == "none" {
        None
    } else {
        match ObjectId::from_hex(query.expected_tip.as_bytes()) {
            Ok(id) => Some(id),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    };

    let result = {
        let store = state.store.lock().await;
        store
            .promote_sync_merge_commit(client_id.clone(), commit_id, expected_branch_tip)
            .await
    };

    match result {
        Ok(()) => {
            // Notify the client's own branch channel (branch was updated).
            state.notify_branches([&client_id]);
            StatusCode::OK.into_response()
        }
        Err(e) if e.downcast_ref::<BranchConflictError>().is_some() => {
            warn!(
                "sync promote-merge: branch conflict for '{}': {e:#}",
                client_id
            );
            StatusCode::CONFLICT.into_response()
        }
        Err(e) => {
            warn!("sync promote-merge: failed for '{}': {e:#}", client_id);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Push endpoint registration ────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterPushEndpointRequest {
    endpoint: String,
}

async fn register_push_endpoint_handler(
    State(state): State<AppState>,
    Extension(AuthedClientId(client_id)): Extension<AuthedClientId>,
    Json(payload): Json<RegisterPushEndpointRequest>,
) -> impl IntoResponse {
    match Url::parse(&payload.endpoint) {
        Ok(url) if url.scheme() == "https" => {}
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }

    match state
        .upsert_push_endpoint(&client_id, payload.endpoint)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            warn!(
                "push register endpoint: failed for '{}': {err:#}",
                client_id
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn delete_push_endpoint_handler(
    State(state): State<AppState>,
    Extension(AuthedClientId(client_id)): Extension<AuthedClientId>,
) -> impl IntoResponse {
    match state.remove_push_endpoint(&client_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            warn!("push delete endpoint: failed for '{}': {err:#}", client_id);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Server startup ─────────────────────────────────────────────────────────────

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/dav/{*path}", any(dav_handler))
        .route("/dav", any(dav_handler))
        .route("/dav/", any(dav_handler))
        .route("/sync/{client_id}/events", get(sync_events_handler))
        .route(
            "/sync/{client_id}/merge-from-main",
            post(sync_merge_from_main_handler),
        )
        .route(
            "/sync/{client_id}/promote-merge/{commit_id}",
            post(sync_promote_merge_handler),
        )
        .route(
            "/push/{client_id}/endpoint",
            post(register_push_endpoint_handler).delete(delete_push_endpoint_handler),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    #[derive(Clone)]
    struct FakePushDelivery {
        statuses: Arc<Mutex<HashMap<String, StatusCode>>>,
        hits: Arc<Mutex<Vec<String>>>,
    }

    impl FakePushDelivery {
        fn new(statuses: impl IntoIterator<Item = (String, StatusCode)>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(statuses.into_iter().collect())),
                hits: Arc::new(Mutex::new(Vec::new())),
            }
        }

        async fn hits(&self) -> Vec<String> {
            self.hits.lock().await.clone()
        }
    }

    impl PushDelivery for FakePushDelivery {
        fn post_branch_updated<'a>(&'a self, endpoint: &'a str) -> PushDeliveryFuture<'a> {
            Box::pin(async move {
                self.hits.lock().await.push(endpoint.to_string());
                Ok(self
                    .statuses
                    .lock()
                    .await
                    .get(endpoint)
                    .copied()
                    .unwrap_or(StatusCode::OK))
            })
        }
    }

    #[tokio::test]
    async fn dispatch_push_notifications_removes_404_and_410_endpoints() {
        let tempdir = TempDir::new().unwrap();
        let config = Config {
            git_store: tempdir.path().join("store.git"),
            bind_addr: "127.0.0.1:0".into(),
            database: crate::config::DatabaseCredentials {
                password: Some("test-password".into()),
                keyfile: None,
            },
            clients: vec![],
        };
        let store = GitStore::open_or_init(&config.git_store).unwrap();
        let delivery = Arc::new(FakePushDelivery::new([
            ("https://push.example/alice".into(), StatusCode::OK),
            ("https://push.example/bob".into(), StatusCode::NOT_FOUND),
            ("https://push.example/carol".into(), StatusCode::GONE),
        ]));
        let state = AppState::new_with_push_delivery(config.clone(), store, delivery.clone());
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap();

        {
            let push_state = state.push_state.lock().await;
            push_state
                .upsert_push_endpoint("alice", "https://push.example/alice".into(), now)
                .unwrap();
            push_state
                .upsert_push_endpoint("bob", "https://push.example/bob".into(), now)
                .unwrap();
            push_state
                .upsert_push_endpoint("carol", "https://push.example/carol".into(), now)
                .unwrap();
        }

        state.dispatch_push_notifications().await.unwrap();

        let hits = delivery.hits().await;
        assert_eq!(
            hits,
            vec![
                "https://push.example/alice".to_string(),
                "https://push.example/bob".to_string(),
                "https://push.example/carol".to_string()
            ]
        );

        let loaded = {
            let push_state = state.push_state.lock().await;
            push_state.load().unwrap()
        };
        assert_eq!(loaded.push_endpoints.len(), 1);
        assert!(loaded.push_endpoints.contains_key("alice"));
    }
}

pub async fn serve_listener(listener: tokio::net::TcpListener, state: AppState) -> Result<()> {
    axum::serve(listener, build_app(state))
        .await
        .wrap_err("server error")?;

    Ok(())
}

pub async fn run_server(config: Config, store: GitStore) -> Result<()> {
    let state = AppState::new(config, store);
    let bind_addr = state.config.bind_addr.clone();

    info!("Listening on http://{bind_addr}");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .wrap_err_with(|| format!("failed to bind to {bind_addr}"))?;

    serve_listener(listener, state).await
}
