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
    add_entry, build_kdbx_bytes, entry_titles, parse_kdbx_bytes, sample_db, test_config, TestServer,
};
use kdbx_git::{
    store::GitStore,
    sync::{sync_local, SyncLocalOptions},
};
use reqwest::{Client, StatusCode};
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
    let (ready_tx, ready_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        kdbx_git::sync::sync_local_with_ready(
            config,
            SyncLocalOptions {
                client_id: "alice".into(),
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
}

struct ProxyServer {
    base_url: String,
    alice_put_count: Arc<AtomicUsize>,
    event_connections: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl ProxyServer {
    async fn start(target_base_url: String, drop_first_events: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let state = ProxyState {
            target_base_url: target_base_url.trim_end_matches('/').to_string(),
            client: Client::builder().build().unwrap(),
            alice_put_count: Arc::new(AtomicUsize::new(0)),
            event_connections: Arc::new(AtomicUsize::new(0)),
            drop_first_events: Arc::new(AtomicBool::new(drop_first_events)),
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

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), proxy.base_url.clone());
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
