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
//!    - `open(read)` → build KDBX bytes from the branch tip
//!    - `open(write)` → accumulate bytes; on `flush`, decrypt and write to git
//!
//! 4. **[`AppState`]** wraps [`GitStore`] in `Arc<tokio::sync::Mutex<...>>`
//!    (step 8) so concurrent writes are serialised.

use std::{
    io::SeekFrom,
    sync::Arc,
    time::SystemTime,
};

use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine};
use bytes::Bytes;
use dav_server::{
    fakels::FakeLs,
    fs::{
        DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
        OpenOptions, ReadDirMeta,
    },
    DavHandler,
};
use eyre::{Context, Result};
use futures_util::stream;
use http::{StatusCode, header};
use tokio::{sync::Mutex, task::spawn_blocking};
use tracing::{info, warn};

use crate::{
    config::Config,
    kdbx::{build_kdbx_sync, parse_kdbx_sync},
    store::GitStore,
};

// ── AppState (step 8) ─────────────────────────────────────────────────────────

/// Shared server state. All fields behind cheap-to-clone handles.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<GitStore>>,
    pub config: Arc<Config>,
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
    fn len(&self) -> u64 { self.len }
    fn modified(&self) -> FsResult<SystemTime> { Ok(self.modified) }
    fn is_dir(&self) -> bool { false }
}

impl DavMetaData for DirMeta {
    fn len(&self) -> u64 { 0 }
    fn modified(&self) -> FsResult<SystemTime> { Ok(SystemTime::now()) }
    fn is_dir(&self) -> bool { true }
}

// ── Directory entry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DbFileEntry;

impl DavDirEntry for DbFileEntry {
    fn name(&self) -> Vec<u8> { b"database.kdbx".to_vec() }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(futures_util::future::ready(Ok(
            Box::new(FileMeta { len: 0, modified: SystemTime::now() }) as Box<dyn DavMetaData>,
        )))
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
        Box::pin(futures_util::future::ready(Ok(
            Box::new(FileMeta { len, modified: SystemTime::now() }) as Box<dyn DavMetaData>,
        )))
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
        Box::pin(futures_util::future::ready(Ok(
            Box::new(FileMeta { len, modified: SystemTime::now() }) as Box<dyn DavMetaData>,
        )))
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
            let all_client_ids: Vec<String> =
                config.clients.iter().map(|c| c.id.clone()).collect();

            // Parse KDBX bytes (blocking crypto work)
            let storage =
                spawn_blocking(move || parse_kdbx_sync(&bytes, &config.database))
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .map_err(|e| {
                        warn!("Client '{}': failed to parse KDBX: {e:#}", client_id);
                        FsError::GeneralFailure
                    })?;

            // Commit to git (serialised by the mutex)
            state
                .store
                .lock()
                .await
                .process_client_write(client_id.clone(), storage, all_client_ids)
                .await
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
                Ok(Box::new(WriteFile { buf: Vec::new(), state, client_id }) as Box<dyn DavFile>)
            } else {
                // Generate KDBX bytes from the branch tip
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

                let bytes =
                    spawn_blocking(move || build_kdbx_sync(&storage, &config.database))
                        .await
                        .map_err(|_| FsError::GeneralFailure)?
                        .map_err(|e| {
                            warn!("Client '{}': failed to build KDBX: {e:#}", client_id);
                            FsError::GeneralFailure
                        })?;

                Ok(Box::new(ReadFile { data: Bytes::from(bytes), pos: 0 }) as Box<dyn DavFile>)
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
                    Ok(Box::new(FileMeta { len: 0, modified: SystemTime::now() })
                        as Box<dyn DavMetaData>)
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

async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let unauthorized = || -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"kdbx-git\"")],
        )
            .into_response()
    };

    // Extract client_id from path: /dav/{client_id}/...
    let path = req.uri().path().to_owned();
    let client_id = path
        .strip_prefix("/dav/")
        .and_then(|s| s.split('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

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
    let found = state.config.clients.iter().any(|c| {
        c.id == client_id && c.username == username && c.password == password
    });

    if found {
        req.extensions_mut().insert(AuthedClientId(client_id));
        next.run(req).await
    } else {
        unauthorized()
    }
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
        .strip_prefix(prefix)
        .build_handler();

    dav.handle(req).await.into_response()
}

// ── Server startup ─────────────────────────────────────────────────────────────

pub async fn run_server(config: Config, store: GitStore) -> Result<()> {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        config: Arc::new(config),
    };

    let bind_addr = state.config.bind_addr.clone();

    let app = Router::new()
        .route("/dav/{*path}", any(dav_handler))
        .route("/dav", any(dav_handler))
        .route("/dav/", any(dav_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    info!("Listening on http://{bind_addr}");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .wrap_err_with(|| format!("failed to bind to {bind_addr}"))?;

    axum::serve(listener, app)
        .await
        .wrap_err("server error")?;

    Ok(())
}
