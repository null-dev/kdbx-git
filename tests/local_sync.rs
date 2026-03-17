mod common;

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header::HOST, Request, Response, StatusCode as HttpStatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use common::{
    add_entry, build_kdbx_bytes, entry_titles, parse_kdbx_bytes, sample_db, test_config,
    write_config, TestServer,
};
use kdbx_git::{
    store::GitStore,
    sync::{sync_local, SyncLocalOptions},
};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
    time::sleep,
};

fn authed(
    client: &Client,
    username: &str,
    password: &str,
    method: reqwest::Method,
    url: &str,
) -> reqwest::RequestBuilder {
    client
        .request(method, url)
        .basic_auth(username, Some(password))
}

async fn wait_for<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if check().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition was not met before timeout"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

fn git(git_dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    let mut command = Command::new("git");
    command.args(["--git-dir", git_dir.to_str().unwrap()]);
    command.args(args);
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
    if let Some(input) = stdin {
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
    }

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn git_commit_parents(git_dir: &Path, rev: &str) -> Vec<String> {
    let line = git(git_dir, &["rev-list", "--parents", "-n", "1", rev], None);
    line.split_whitespace()
        .skip(1)
        .map(|part| part.to_string())
        .collect()
}

fn spawn_sync(
    config: kdbx_git::config::Config,
    local_path: std::path::PathBuf,
    server_url: String,
) -> (JoinHandle<()>, oneshot::Receiver<()>) {
    spawn_sync_for("alice", config, local_path, server_url)
}

fn spawn_sync_for(
    client_id: &str,
    config: kdbx_git::config::Config,
    local_path: std::path::PathBuf,
    server_url: String,
) -> (JoinHandle<()>, oneshot::Receiver<()>) {
    let client_id = client_id.to_string();
    let (ready_tx, ready_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        kdbx_git::sync::sync_local_with_ready(
            config,
            SyncLocalOptions {
                client_id,
                local_path,
                once: false,
                poll: false,
                server_url: Some(server_url),
            },
            ready_tx,
        )
        .await
        .unwrap();
    });
    (handle, ready_rx)
}

#[derive(Clone)]
struct ProxyState {
    target_base_url: String,
    client: Client,
    alice_put_count: Arc<AtomicUsize>,
    event_connections: Arc<AtomicUsize>,
    drop_first_events: Arc<AtomicBool>,
    alice_promote_status: Option<HttpStatusCode>,
}

struct ProxyServer {
    base_url: String,
    alice_put_count: Arc<AtomicUsize>,
    event_connections: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl ProxyServer {
    async fn start(target_base_url: String, drop_first_events: bool) -> Self {
        Self::start_with_options(target_base_url, drop_first_events, None).await
    }

    async fn start_with_options(
        target_base_url: String,
        drop_first_events: bool,
        alice_promote_status: Option<HttpStatusCode>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let state = ProxyState {
            target_base_url: target_base_url.trim_end_matches('/').to_string(),
            client: Client::builder().build().unwrap(),
            alice_put_count: Arc::new(AtomicUsize::new(0)),
            event_connections: Arc::new(AtomicUsize::new(0)),
            drop_first_events: Arc::new(AtomicBool::new(drop_first_events)),
            alice_promote_status,
        };

        let app = Router::new()
            .route("/", any(proxy_handler))
            .route("/{*path}", any(proxy_handler))
            .with_state(state.clone());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            base_url,
            alice_put_count: state.alice_put_count,
            event_connections: state.event_connections,
            handle,
        }
    }

    fn alice_put_count(&self) -> usize {
        self.alice_put_count.load(Ordering::SeqCst)
    }

    fn event_connections(&self) -> usize {
        self.event_connections.load(Ordering::SeqCst)
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn proxy_handler(State(state): State<ProxyState>, req: Request<Body>) -> impl IntoResponse {
    let path = req.uri().path().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| path.clone());

    if path == "/sync/alice/events" {
        state.event_connections.fetch_add(1, Ordering::SeqCst);
        if state.drop_first_events.swap(false, Ordering::SeqCst) {
            return Response::builder()
                .status(HttpStatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from("event: ready\ndata: 0\n\n"))
                .unwrap();
        }
    }

    if req.method() == reqwest::Method::PUT && path == "/dav/alice/database.kdbx" {
        state.alice_put_count.fetch_add(1, Ordering::SeqCst);
    }

    if req.method() == reqwest::Method::POST
        && path.starts_with("/sync/alice/promote-merge/")
        && state.alice_promote_status.is_some()
    {
        return Response::builder()
            .status(state.alice_promote_status.unwrap())
            .body(Body::empty())
            .unwrap();
    }

    let target_url = format!("{}{}", state.target_base_url, path_and_query);
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap();

    let mut upstream = state.client.request(parts.method, target_url);
    for (name, value) in &parts.headers {
        if name != HOST {
            upstream = upstream.header(name, value);
        }
    }

    let response = match upstream.body(body).send().await {
        Ok(response) => response,
        Err(err) => {
            return Response::builder()
                .status(HttpStatusCode::BAD_GATEWAY)
                .body(Body::from(format!("proxy error: {err}")))
                .unwrap();
        }
    };

    let status = response.status();
    let headers = response.headers().clone();
    let stream = response.bytes_stream();
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from_stream(stream)).unwrap()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TestSyncState {
    pending_promote: Option<TestPendingPromote>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TestPendingPromote {
    commit_id: String,
    expected_branch_tip: Option<String>,
}

fn sync_state_path(local_path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.sync-state.json", local_path.display()))
}

async fn load_sync_state(local_path: &Path) -> TestSyncState {
    let text = tokio::fs::read_to_string(sync_state_path(local_path))
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

async fn write_sync_state(local_path: &Path, state: &TestSyncState) {
    tokio::fs::write(
        sync_state_path(local_path),
        serde_json::to_vec(state).unwrap(),
    )
    .await
    .unwrap();
}

async fn request_pending_promote(client: &Client, base_url: &str) -> (Vec<u8>, TestPendingPromote) {
    let response = authed(
        client,
        "alice-user",
        "alice-pass",
        reqwest::Method::POST,
        &format!("{}/sync/alice/merge-from-main", base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let commit_id = response
        .headers()
        .get("X-Merge-Commit-Id")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_string();
    let expected_tip = response
        .headers()
        .get("X-Expected-Branch-Tip")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            if value == "none" {
                None
            } else {
                Some(value.to_string())
            }
        });
    let bytes = response.bytes().await.unwrap().to_vec();

    (
        bytes,
        TestPendingPromote {
            commit_id,
            expected_branch_tip: expected_tip,
        },
    )
}

/// When alice's branch doesn't exist but main has content, sync-local --once
/// should create alice's branch and write the merged content to the local file.
#[serial_test::serial]
#[tokio::test]
async fn sync_local_creates_branch_and_pulls_from_main() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    // Bob writes to main via WebDAV; alice's branch is not yet created.
    let bob_db = sample_db("Bob DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    // Alice's sync-local --once: should create alice's branch via the sync
    // merge endpoints and write the local KDBX file.
    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    let bytes = tokio::fs::read(&local_path).await.unwrap();
    let parsed = parse_kdbx_bytes(&bytes, &config.database);
    assert!(
        entry_titles(&parsed).contains(&"Bob Entry".to_string()),
        "local file should contain Bob's entry; got: {:?}",
        entry_titles(&parsed)
    );
}

/// When alice's branch is behind main, sync-local should pull the new content
/// from the server and keep the local file up to date as main continues to
/// advance (SSE-driven).
#[serial_test::serial]
#[tokio::test]
async fn sync_local_updates_local_file_when_main_advances() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let client = Client::new();

    // Alice writes the initial database so her branch and main start together.
    let alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(alice_put.status().is_success());

    // Bob forks from main (GET merges main into bob's branch), then adds an
    // entry and writes back.  Main now contains both entries; alice is behind.
    let bob_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let mut bob_db = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000020",
        "Bob Entry",
        "bob",
        "bobpass",
    );
    let bob_put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put.status().is_success());

    // Start alice's continuous sync-local; the initial reconcile should pull
    // the current main state (alice + bob entries) into the local file.
    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Bob Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    // Bob writes again with an extra entry; main advances and SSE fires.
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000021",
        "Bob Extra Entry",
        "bob",
        "bobpass",
    );
    let bob_put2 = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put2.status().is_success());

    // alice's sync-local should react to the SSE event and write the update.
    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Bob Extra Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    sync_task.abort();
    let _ = sync_task.await;
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_once_when_already_up_to_date_does_not_modify_local_file() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let alice_db = sample_db("Alice DB", "Alice Entry");
    let put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    let sentinel = b"leave-local-file-alone".to_vec();
    tokio::fs::write(&local_path, &sentinel).await.unwrap();

    sync_local(
        config,
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    assert_eq!(tokio::fs::read(&local_path).await.unwrap(), sentinel);
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_once_when_main_does_not_exist_exits_without_creating_local_file() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");

    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    assert!(!tokio::fs::try_exists(&local_path).await.unwrap());

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    assert!(store.branch_tip_id("main".into()).await.unwrap().is_none());
    assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_processes_multiple_rapid_sse_updates() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let client = Client::new();

    let alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(alice_put.status().is_success());

    let bob_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let mut bob_db = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000022",
        "Burst Entry 1",
        "bob",
        "bobpass",
    );
    let bob_put1 = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put1.status().is_success());

    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000023",
        "Burst Entry 2",
        "bob",
        "bobpass",
    );
    let bob_put2 = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put2.status().is_success());

    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000024",
        "Burst Entry 3",
        "bob",
        "bobpass",
    );
    let bob_put3 = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put3.status().is_success());

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => {
                    let titles = entry_titles(&parse_kdbx_bytes(&bytes, &database));
                    titles.contains(&"Burst Entry 1".to_string())
                        && titles.contains(&"Burst Entry 2".to_string())
                        && titles.contains(&"Burst Entry 3".to_string())
                }
                Err(_) => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_pull_writes_valid_kdbx_file() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let bob_db = sample_db("Bob DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    let bytes = tokio::fs::read(&local_path).await.unwrap();
    let parsed = parse_kdbx_bytes(&bytes, &config.database);
    assert!(
        entry_titles(&parsed).contains(&"Bob Entry".to_string()),
        "local file should be a readable KDBX with Bob's entry; got {:?}",
        entry_titles(&parsed)
    );
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_promotes_pull_result_onto_alice_branch() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let base_db = sample_db("Base DB", "Shared Entry");
    store
        .commit_to_branch("main".into(), base_db.clone(), "seed main".into())
        .await
        .unwrap();

    let mut alice_db = base_db.clone();
    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000025",
        "Alice Branch Entry",
        "alice",
        "alicepass",
    );
    store
        .commit_to_branch("alice".into(), alice_db, "seed alice divergence".into())
        .await
        .unwrap();

    let mut main_db = base_db.clone();
    add_entry(
        &mut main_db,
        "00000000-0000-0000-0000-000000000026",
        "Main Branch Entry",
        "bob",
        "bobpass",
    );
    store
        .commit_to_branch("main".into(), main_db, "advance main".into())
        .await
        .unwrap();

    let old_alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();

    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");

    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    let new_alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    assert_ne!(new_alice_tip, old_alice_tip);
    assert_ne!(new_alice_tip, main_tip);

    let parents = git_commit_parents(&config.git_store, &new_alice_tip.to_hex().to_string());
    assert_eq!(
        parents.len(),
        2,
        "expected promoted merge commit, got {parents:?}"
    );
    assert!(parents.contains(&old_alice_tip.to_hex().to_string()));
    assert!(parents.contains(&main_tip.to_hex().to_string()));
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_pull_followed_by_merge_from_main_returns_204() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let bob_db = sample_db("Bob DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    let response = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::POST,
        &format!("{}/sync/alice/merge-from-main", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_pull_writes_local_file_atomically() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let bob_db = sample_db("Atomic DB", "Initial Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    let bob_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let mut bob_db = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    for idx in 0..150 {
        add_entry(
            &mut bob_db,
            &format!("00000000-0000-0000-0000-00000000{:04x}", 0x300 + idx),
            &format!("Atomic Entry {idx}"),
            "bob",
            "bobpass",
        );
    }

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    let failures = Arc::new(Mutex::new(Vec::<String>::new()));
    let (stop_tx, stop_rx) = watch::channel(false);
    let observer_failures = failures.clone();
    let observer_path = local_path.clone();
    let observer_database = config.database.clone();
    let observer = tokio::spawn(async move {
        let mut stop_rx = stop_rx;
        loop {
            if *stop_rx.borrow() {
                break;
            }

            match tokio::fs::read(&observer_path).await {
                Ok(bytes) => {
                    if let Err(err) =
                        std::panic::catch_unwind(|| parse_kdbx_bytes(&bytes, &observer_database))
                    {
                        observer_failures
                            .lock()
                            .unwrap()
                            .push(format!("observer saw unreadable file: {err:?}"));
                        break;
                    }
                }
                Err(err) => {
                    observer_failures
                        .lock()
                        .unwrap()
                        .push(format!("observer failed to read file: {err}"));
                    break;
                }
            }

            tokio::select! {
                _ = sleep(Duration::from_millis(10)) => {}
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
    });

    let bob_put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put.status().is_success());

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Atomic Entry 149".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    let _ = stop_tx.send(true);
    observer.await.unwrap();
    sync_task.abort();

    let failures = failures.lock().unwrap().clone();
    assert!(
        failures.is_empty(),
        "atomic-write observer failures: {failures:?}"
    );
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_pull_does_not_immediately_push_file_back_to_server() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let proxy = ProxyServer::start(server.base_url.clone(), false).await;
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let bob_db = sample_db("Bob DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    let (sync_task, _ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), proxy.base_url.clone());

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Bob Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    sleep(Duration::from_millis(1200)).await;
    sync_task.abort();

    assert_eq!(
        proxy.alice_put_count(),
        0,
        "sync-local should suppress self-write PUTs"
    );
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_reconnects_sse_and_receives_later_updates() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let proxy = ProxyServer::start(server.base_url.clone(), true).await;
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(alice_put.status().is_success());

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), proxy.base_url.clone());
    ready_rx.await.unwrap();

    wait_for(|| {
        let proxy = &proxy;
        async move { proxy.event_connections() >= 2 }
    })
    .await;

    let bob_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let mut bob_db = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000027",
        "Reconnect Entry",
        "bob",
        "bobpass",
    );
    let bob_put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put.status().is_success());

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Reconnect Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    sync_task.abort();
    assert!(proxy.event_connections() >= 2);
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_local_edits_are_uploaded_via_webdav_put() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let proxy = ProxyServer::start(server.base_url.clone(), false).await;
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), proxy.base_url.clone());
    ready_rx.await.unwrap();

    let alice_db = sample_db("Local Push DB", "Alice Local Entry");
    tokio::fs::write(&local_path, build_kdbx_bytes(&alice_db, &config.database))
        .await
        .unwrap();

    wait_for(|| {
        let proxy = &proxy;
        async move { proxy.alice_put_count() >= 1 }
    })
    .await;

    wait_for(|| {
        let client = client.clone();
        let base_url = server.base_url.clone();
        let database = config.database.clone();
        async move {
            match authed(
                &client,
                "alice-user",
                "alice-pass",
                reqwest::Method::GET,
                &format!("{}/dav/alice/database.kdbx", base_url),
            )
            .send()
            .await
            {
                Ok(response) if response.status() == StatusCode::OK => {
                    let bytes = response.bytes().await.unwrap();
                    entry_titles(&parse_kdbx_bytes(&bytes, &database))
                        .contains(&"Alice Local Entry".to_string())
                }
                _ => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_local_push_advances_main() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    let alice_db = sample_db("Local Push DB", "Alice Main Entry");
    tokio::fs::write(&local_path, build_kdbx_bytes(&alice_db, &config.database))
        .await
        .unwrap();

    wait_for(|| {
        let store = &store;
        async move {
            match store.read_branch("main".into()).await.unwrap() {
                Some(db) => entry_titles(&db).contains(&"Alice Main Entry".to_string()),
                None => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_push_pulls_back_round_tripped_merged_result() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let bob_db = sample_db("Shared DB", "Bob Entry");
    let bob_put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(bob_put.status().is_success());

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Bob Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    let mut alice_local = parse_kdbx_bytes(
        &tokio::fs::read(&local_path).await.unwrap(),
        &config.database,
    );
    add_entry(
        &mut alice_local,
        "00000000-0000-0000-0000-000000000028",
        "Alice Local Entry",
        "alice",
        "alicepass",
    );
    tokio::fs::write(
        &local_path,
        build_kdbx_bytes(&alice_local, &config.database),
    )
    .await
    .unwrap();

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => {
                    let titles = entry_titles(&parse_kdbx_bytes(&bytes, &database));
                    titles.contains(&"Bob Entry".to_string())
                        && titles.contains(&"Alice Local Entry".to_string())
                }
                Err(_) => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_identical_resave_does_not_create_server_commit() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    let alice_bytes =
        build_kdbx_bytes(&sample_db("Local Push DB", "Alice Entry"), &config.database);
    tokio::fs::write(&local_path, &alice_bytes).await.unwrap();

    wait_for(|| {
        let store = &store;
        async move { store.branch_tip_id("alice".into()).await.unwrap().is_some() }
    })
    .await;

    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_before = store.branch_tip_id("main".into()).await.unwrap().unwrap();

    tokio::fs::write(&local_path, &alice_bytes).await.unwrap();
    sleep(Duration::from_millis(1200)).await;

    let alice_tip_after = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_after = store.branch_tip_id("main".into()).await.unwrap().unwrap();

    assert_eq!(alice_tip_after, alice_tip_before);
    assert_eq!(main_tip_after, main_tip_before);

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_alice_push_eventually_updates_bobs_local_file() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let alice_local_path = server.temp_root().join("alice-local.kdbx");
    let bob_local_path = server.temp_root().join("bob-local.kdbx");

    let (alice_sync_task, alice_ready_rx) = spawn_sync_for(
        "alice",
        config.clone(),
        alice_local_path.clone(),
        server.base_url.clone(),
    );
    let (bob_sync_task, bob_ready_rx) = spawn_sync_for(
        "bob",
        config.clone(),
        bob_local_path.clone(),
        server.base_url.clone(),
    );
    alice_ready_rx.await.unwrap();
    bob_ready_rx.await.unwrap();

    let alice_db = sample_db("Two Clients DB", "Alice Shared Entry");
    tokio::fs::write(
        &alice_local_path,
        build_kdbx_bytes(&alice_db, &config.database),
    )
    .await
    .unwrap();

    wait_for(|| {
        let bob_local_path = bob_local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&bob_local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Alice Shared Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    alice_sync_task.abort();
    bob_sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_rapid_local_saves_are_debounced_into_single_put() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let proxy = ProxyServer::start(server.base_url.clone(), false).await;
    let local_path = server.temp_root().join("alice-local.kdbx");

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), proxy.base_url.clone());
    ready_rx.await.unwrap();

    for idx in 0..3 {
        let mut db = sample_db("Debounce DB", &format!("Alice Save {idx}"));
        add_entry(
            &mut db,
            &format!("00000000-0000-0000-0000-00000000003{idx}"),
            &format!("Extra {idx}"),
            "alice",
            "alicepass",
        );
        tokio::fs::write(&local_path, build_kdbx_bytes(&db, &config.database))
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;
    }

    wait_for(|| {
        let proxy = &proxy;
        async move { proxy.alice_put_count() >= 1 }
    })
    .await;
    sleep(Duration::from_millis(1200)).await;

    assert_eq!(
        proxy.alice_put_count(),
        1,
        "rapid local saves should coalesce into one PUT"
    );

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_missing_local_file_on_push_event_does_not_crash() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let initial_db = sample_db("Missing File DB", "Initial Entry");
    tokio::fs::write(&local_path, build_kdbx_bytes(&initial_db, &config.database))
        .await
        .unwrap();

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    tokio::fs::remove_file(&local_path).await.unwrap();
    sleep(Duration::from_millis(800)).await;
    assert!(
        !sync_task.is_finished(),
        "sync-local should ignore missing local file during push"
    );

    let replacement_db = sample_db("Missing File DB", "Replacement Entry");
    tokio::fs::write(
        &local_path,
        build_kdbx_bytes(&replacement_db, &config.database),
    )
    .await
    .unwrap();

    wait_for(|| {
        let client = client.clone();
        let base_url = server.base_url.clone();
        let database = config.database.clone();
        async move {
            match authed(
                &client,
                "alice-user",
                "alice-pass",
                reqwest::Method::GET,
                &format!("{}/dav/alice/database.kdbx", base_url),
            )
            .send()
            .await
            {
                Ok(response) if response.status() == StatusCode::OK => {
                    let bytes = response.bytes().await.unwrap();
                    entry_titles(&parse_kdbx_bytes(&bytes, &database))
                        .contains(&"Replacement Entry".to_string())
                }
                _ => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_preexisting_local_file_is_pushed_on_first_start() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let local_db = sample_db("Startup Push DB", "Preexisting Local Entry");
    tokio::fs::write(&local_path, build_kdbx_bytes(&local_db, &config.database))
        .await
        .unwrap();

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    wait_for(|| {
        let client = client.clone();
        let base_url = server.base_url.clone();
        let database = config.database.clone();
        async move {
            match authed(
                &client,
                "alice-user",
                "alice-pass",
                reqwest::Method::GET,
                &format!("{}/dav/alice/database.kdbx", base_url),
            )
            .send()
            .await
            {
                Ok(response) if response.status() == StatusCode::OK => {
                    let bytes = response.bytes().await.unwrap();
                    entry_titles(&parse_kdbx_bytes(&bytes, &database))
                        .contains(&"Preexisting Local Entry".to_string())
                }
                _ => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_persists_pending_promote_state_before_promote_completes() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let proxy = ProxyServer::start_with_options(
        server.base_url.clone(),
        false,
        Some(HttpStatusCode::INTERNAL_SERVER_ERROR),
    )
    .await;
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let config_path = server.temp_root().join("config.toml");
    write_config(&config_path, &config);

    let bob_db = sample_db("Recovery DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    let mut sync_process = Command::new(env!("CARGO_BIN_EXE_kdbx-git"))
        .args([
            "sync-local",
            "--server-url",
            &proxy.base_url,
            config_path.to_str().unwrap(),
            "alice",
            local_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    sleep(Duration::from_millis(750)).await;

    let state = load_sync_state(&local_path).await;
    let pending = state
        .pending_promote
        .expect("pending promote should be persisted");
    assert!(!pending.commit_id.is_empty());

    let local_bytes = tokio::fs::read(&local_path).await.unwrap();
    let local_db = parse_kdbx_bytes(&local_bytes, &config.database);
    assert!(entry_titles(&local_db).contains(&"Bob Entry".to_string()));

    let _ = sync_process.kill();
    let _ = sync_process.wait();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_recovers_pending_promote_and_clears_state_file() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let bob_db = sample_db("Recovery DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    let (bytes, pending) = request_pending_promote(&client, &server.base_url).await;
    tokio::fs::write(&local_path, bytes).await.unwrap();
    write_sync_state(
        &local_path,
        &TestSyncState {
            pending_promote: Some(pending.clone()),
        },
    )
    .await;

    sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .unwrap();

    let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip.to_hex().to_string(), pending.commit_id);

    let state = load_sync_state(&local_path).await;
    assert!(
        state.pending_promote.is_none(),
        "state file should be cleared"
    );

    let get = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::GET,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let fetched = parse_kdbx_bytes(&get.bytes().await.unwrap(), &config.database);
    assert!(entry_titles(&fetched).contains(&"Bob Entry".to_string()));
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_recovery_branch_conflict_is_fatal() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let bob_db = sample_db("Recovery DB", "Bob Entry");
    let put = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(put.status().is_success());

    let (bytes, pending) = request_pending_promote(&client, &server.base_url).await;
    tokio::fs::write(&local_path, bytes).await.unwrap();
    write_sync_state(
        &local_path,
        &TestSyncState {
            pending_promote: Some(pending),
        },
    )
    .await;

    let alice_db = sample_db("Conflict DB", "Alice Branch Entry");
    let alice_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(alice_put.status().is_success());

    let err = sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .expect_err("recovery should fail fatally on branch conflict");
    let message = format!("{err:#}");
    assert!(message.contains("failed to recover pending promote"));
    assert!(message.contains("branch was modified unexpectedly"));
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_stale_pending_promote_reports_useful_error() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");

    let stale_pending = TestPendingPromote {
        commit_id: "1111111111111111111111111111111111111111".into(),
        expected_branch_tip: None,
    };
    write_sync_state(
        &local_path,
        &TestSyncState {
            pending_promote: Some(stale_pending.clone()),
        },
    )
    .await;

    let err = sync_local(
        config.clone(),
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            poll: false,
            server_url: Some(server.base_url.clone()),
        },
    )
    .await
    .expect_err("stale pending promote should fail with a useful error");
    let message = format!("{err:#}");
    assert!(message.contains("failed to recover pending promote"));
    assert!(
        message.contains(&stale_pending.commit_id),
        "error should reference stale commit id: {message}"
    );
    assert!(
        message.contains("unexpected status from promote-merge")
            || message.contains("500 Internal Server Error"),
        "error should explain the recovery failure: {message}"
    );
}
