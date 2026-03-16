mod common;

use std::time::Duration;

use common::{
    add_entry, entry_titles, parse_kdbx_bytes, sample_db, test_config, write_source_kdbx,
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

#[serial_test::serial]
#[tokio::test]
async fn sync_local_pushes_local_file_into_server_over_http() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let local_db = sample_db("Local DB", "Local Entry");
    let client = Client::new();

    write_source_kdbx(&local_path, &local_db, &config.database);

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
        reqwest::Method::GET,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let parsed = parse_kdbx_bytes(&response.bytes().await.unwrap(), &config.database);
    assert_eq!(entry_titles(&parsed), vec!["Local Entry".to_string()]);
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_pulls_server_state_into_missing_file_over_http() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let remote_db = sample_db("Remote DB", "Remote Entry");
    let client = Client::new();

    let upload = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(common::build_kdbx_bytes(&remote_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(upload.status().is_success());

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

    let bytes = tokio::fs::read(&local_path).await.unwrap();
    let parsed = parse_kdbx_bytes(&bytes, &config.database);
    assert_eq!(entry_titles(&parsed), vec!["Remote Entry".to_string()]);
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_reacts_to_remote_server_updates() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let initial_db = sample_db("Initial DB", "Shared Entry");
    let mut updated_db = sample_db("Updated DB", "Shared Entry");
    add_entry(
        &mut updated_db,
        "00000000-0000-0000-0000-000000000030",
        "Updated Entry",
        "alice",
        "updated-pass",
    );
    let client = Client::new();

    write_source_kdbx(&local_path, &initial_db, &config.database);
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

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();

    let upload = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(common::build_kdbx_bytes(&updated_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(upload.status().is_success());

    wait_for(|| {
        let local_path = local_path.clone();
        let database = config.database.clone();
        async move {
            match tokio::fs::read(&local_path).await {
                Ok(bytes) => entry_titles(&parse_kdbx_bytes(&bytes, &database))
                    .contains(&"Updated Entry".to_string()),
                Err(_) => false,
            }
        }
    })
    .await;

    sync_task.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn sync_local_reacts_to_local_file_updates() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let local_path = server.temp_root().join("alice-local.kdbx");
    let initial_db = sample_db("Initial DB", "Shared Entry");
    let mut updated_db = sample_db("Updated DB", "Shared Entry");
    add_entry(
        &mut updated_db,
        "00000000-0000-0000-0000-000000000031",
        "Updated Entry",
        "alice",
        "updated-pass",
    );
    let client = Client::new();

    write_source_kdbx(&local_path, &initial_db, &config.database);
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

    let (sync_task, ready_rx) =
        spawn_sync(config.clone(), local_path.clone(), server.base_url.clone());
    ready_rx.await.unwrap();
    write_source_kdbx(&local_path, &updated_db, &config.database);

    wait_for(|| {
        let client = client.clone();
        let database = config.database.clone();
        let url = format!("{}/dav/alice/database.kdbx", server.base_url);
        async move {
            match authed(
                &client,
                "alice-user",
                "alice-pass",
                reqwest::Method::GET,
                &url,
            )
            .send()
            .await
            {
                Ok(response) if response.status() == StatusCode::OK => {
                    let bytes = response.bytes().await.unwrap();
                    entry_titles(&parse_kdbx_bytes(&bytes, &database))
                        .contains(&"Updated Entry".to_string())
                }
                _ => false,
            }
        }
    })
    .await;

    sync_task.abort();
}
