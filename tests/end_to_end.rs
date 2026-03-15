mod common;

use std::process::Command;

use common::{
    add_entry, build_kdbx_bytes, entry_titles, parse_kdbx_bytes, sample_db, test_config,
    write_config, write_source_kdbx, TestServer, MASTER_PASSWORD,
};
use kdbx_git::{init::init_from_config_path, store::GitStore};
use reqwest::{header, Client, StatusCode};
use tempfile::TempDir;

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

#[tokio::test]
async fn init_imports_main_and_git_history_is_readable() {
    let tempdir = TempDir::new().unwrap();
    let source_db = sample_db("Imported DB", "Imported Entry");
    let source_path = tempdir.path().join("source.kdbx");
    let config = test_config(tempdir.path(), Some(source_path.clone()));
    let config_path = tempdir.path().join("config.toml");

    write_source_kdbx(&source_path, &source_db, &config.database);
    write_config(&config_path, &config);

    init_from_config_path(&config_path).await.unwrap();

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let main = store
        .read_branch("main".into())
        .await
        .unwrap()
        .expect("main branch should exist after init");
    assert_eq!(entry_titles(&main), vec!["Imported Entry".to_string()]);

    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let response = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.bytes().await.unwrap();
    let bob_view = parse_kdbx_bytes(&body, &server.config.database);
    assert_eq!(entry_titles(&bob_view), vec!["Imported Entry".to_string()]);

    let git_log = Command::new("git")
        .args([
            "--git-dir",
            server.config.git_store.to_str().unwrap(),
            "log",
            "--stat",
            "--format=%s",
            "main",
        ])
        .output()
        .unwrap();
    assert!(git_log.status.success());

    let stdout = String::from_utf8(git_log.stdout).unwrap();
    assert!(stdout.contains("import"));
    assert!(stdout.contains("db.json"));
}

#[tokio::test]
async fn client_writes_merge_and_fan_out_across_clients() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_bytes = build_kdbx_bytes(&alice_db, &config.database);
    let alice_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(alice_bytes)
    .send()
    .await
    .unwrap();
    assert!(
        alice_put.status().is_success(),
        "unexpected PUT status: {}",
        alice_put.status()
    );

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
    assert!(entry_titles(&bob_db).contains(&"Alice Entry".to_string()));

    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000011",
        "Bob Entry",
        "bob",
        "tr0ub4dor",
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
    assert!(
        bob_put.status().is_success(),
        "unexpected PUT status: {}",
        bob_put.status()
    );

    let alice_get = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::GET,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(alice_get.status(), StatusCode::OK);

    let merged = parse_kdbx_bytes(&alice_get.bytes().await.unwrap(), &config.database);
    let titles = entry_titles(&merged);
    assert!(
        titles.contains(&"Alice Entry".to_string()),
        "titles were {titles:?}"
    );
    assert!(
        titles.contains(&"Bob Entry".to_string()),
        "titles were {titles:?}"
    );
}

#[tokio::test]
async fn malformed_uploads_and_wrong_kdbx_password_are_rejected() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let malformed = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(vec![1_u8, 2, 3, 4, 5])
    .send()
    .await
    .unwrap();
    assert_eq!(malformed.status(), StatusCode::FORBIDDEN);

    let wrong_bytes = build_kdbx_bytes(
        &sample_db("Wrong Password DB", "Rejected Entry"),
        &kdbx_git::config::DatabaseCredentials {
            path: None,
            password: Some(format!("{MASTER_PASSWORD}-wrong")),
            keyfile: None,
        },
    );
    let wrong_password = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(wrong_bytes)
    .send()
    .await
    .unwrap();
    assert_eq!(wrong_password.status(), StatusCode::FORBIDDEN);

    let store = GitStore::open_or_init(&server.config.git_store).unwrap();
    assert!(store.read_branch("alice".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn auth_failures_return_basic_auth_challenge() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let no_auth = client
        .get(format!("{}/dav/alice/database.kdbx", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        no_auth
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"kdbx-git\"")
    );

    let wrong_auth = authed(
        &client,
        "alice-user",
        "wrong-pass",
        reqwest::Method::GET,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(wrong_auth.status(), StatusCode::UNAUTHORIZED);
}
