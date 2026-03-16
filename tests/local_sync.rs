mod common;

use std::time::Duration;

use common::{
    add_entry, build_kdbx_bytes, entry_titles, parse_kdbx_bytes, sample_db, test_config,
    TestServer,
};
use kdbx_git::sync::{sync_local, SyncLocalOptions};
use reqwest::{Client, StatusCode};
use tempfile::TempDir;
use tokio::{sync::oneshot, task::JoinHandle, time::sleep};

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
