mod common;

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

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

fn authed_propfind(
    client: &Client,
    username: &str,
    password: &str,
    url: &str,
    depth: &str,
) -> reqwest::RequestBuilder {
    authed(
        client,
        username,
        password,
        reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
        url,
    )
    .header("Depth", depth)
    .header(header::CONTENT_TYPE, "application/xml")
    .body(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:allprop/>
</D:propfind>"#,
    )
}

fn assert_storage_eq(
    actual: &kdbx_git::storage::types::StorageDatabase,
    expected: &kdbx_git::storage::types::StorageDatabase,
) {
    let actual = serde_json::to_value(actual).unwrap();
    let expected = serde_json::to_value(expected).unwrap();
    assert_eq!(actual, expected);
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

fn replace_main_with_corrupt_commit(git_dir: &Path, parent: &str) {
    let blob_id = git(
        git_dir,
        &["hash-object", "-w", "--stdin"],
        Some(br#"{"broken":"#),
    );
    let tree_spec = format!("100644 blob {blob_id}\tdb.json\n");
    let tree_id = git(git_dir, &["mktree"], Some(tree_spec.as_bytes()));

    let mut command = Command::new("git");
    command
        .args([
            "--git-dir",
            git_dir.to_str().unwrap(),
            "commit-tree",
            &tree_id,
            "-p",
            parent,
            "-m",
            "corrupt main",
        ])
        .env("GIT_AUTHOR_NAME", "Tests")
        .env("GIT_AUTHOR_EMAIL", "tests@example.com")
        .env("GIT_COMMITTER_NAME", "Tests")
        .env("GIT_COMMITTER_EMAIL", "tests@example.com");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git commit-tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit_id = String::from_utf8(output.stdout).unwrap().trim().to_string();

    let status = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().unwrap(),
            "update-ref",
            "refs/heads/main",
            &commit_id,
            parent,
        ])
        .status()
        .unwrap();
    assert!(status.success(), "git update-ref failed");
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
async fn get_when_client_branch_does_not_yet_exist_returns_404() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

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

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn get_when_only_client_branch_exists_returns_clients_own_content() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_db = sample_db("Alice Only DB", "Alice Only Entry");
    store
        .commit_to_branch("alice".into(), alice_db.clone(), "seed alice".into())
        .await
        .unwrap();

    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

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

    let fetched = parse_kdbx_bytes(&response.bytes().await.unwrap(), &config.database);
    assert_storage_eq(&fetched, &alice_db);

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    assert!(store.branch_tip_id("main".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn get_after_clients_own_put_returns_that_same_content() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Alice Round Trip", "Alice Entry");
    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000012",
        "Alice Extra Entry",
        "alice",
        "s3cr3t",
    );

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
    assert!(
        put.status().is_success(),
        "unexpected PUT status: {}",
        put.status()
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
    assert_storage_eq(&fetched, &alice_db);
}

#[tokio::test]
async fn get_always_includes_content_from_main_even_when_client_never_wrote_anything() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let bob_db = sample_db("Shared DB", "Bob Entry");
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
    assert!(
        put.status().is_success(),
        "unexpected PUT status: {}",
        put.status()
    );

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());
    let main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();

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

    let fetched = parse_kdbx_bytes(&response.bytes().await.unwrap(), &config.database);
    assert!(entry_titles(&fetched).contains(&"Bob Entry".to_string()));

    let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip, main_tip);
}

#[tokio::test]
async fn get_triggers_merge_on_read_when_client_branch_is_behind_main() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Shared DB", "Alice Entry");
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

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let bob_tip_before = store.branch_tip_id("bob".into()).await.unwrap().unwrap();
    let bob_before = store.read_branch("bob".into()).await.unwrap().unwrap();
    assert_eq!(entry_titles(&bob_before), vec!["Alice Entry".to_string()]);

    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000013",
        "Alice New Entry",
        "alice",
        "new-secret",
    );
    let alice_put_2 = authed(
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
    assert!(alice_put_2.status().is_success());

    let bob_get_2 = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_get_2.status(), StatusCode::OK);

    let fetched = parse_kdbx_bytes(&bob_get_2.bytes().await.unwrap(), &config.database);
    let fetched_titles = entry_titles(&fetched);
    assert!(fetched_titles.contains(&"Alice Entry".to_string()));
    assert!(fetched_titles.contains(&"Alice New Entry".to_string()));

    let bob_tip_after = store.branch_tip_id("bob".into()).await.unwrap().unwrap();
    assert_ne!(bob_tip_before, bob_tip_after);

    let bob_after = store.read_branch("bob".into()).await.unwrap().unwrap();
    let bob_titles = entry_titles(&bob_after);
    assert!(bob_titles.contains(&"Alice Entry".to_string()));
    assert!(bob_titles.contains(&"Alice New Entry".to_string()));
}

#[tokio::test]
async fn get_when_merge_on_read_fails_still_returns_clients_stale_data() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    store
        .commit_to_branch(
            "main".into(),
            sample_db("Seed Main", "Seed Entry"),
            "seed main".into(),
        )
        .await
        .unwrap();
    store.ensure_client_branch("alice".into()).await.unwrap();

    let alice_stale = sample_db("Alice Stale", "Alice Stale Entry");
    store
        .commit_to_branch(
            "alice".into(),
            alice_stale.clone(),
            "seed stale alice".into(),
        )
        .await
        .unwrap();
    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_before = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    replace_main_with_corrupt_commit(&config.git_store, &main_tip_before.to_hex().to_string());

    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

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

    let fetched = parse_kdbx_bytes(&response.bytes().await.unwrap(), &config.database);
    assert_storage_eq(&fetched, &alice_stale);

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_tip_after = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip_after, alice_tip_before);
}

#[tokio::test]
async fn get_on_directory_path_returns_multistatus_listing_containing_database_kdbx() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::GET,
        &format!("{}/dav/alice/", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status().as_u16(), 207);

    let body = response.text().await.unwrap();
    assert!(body.contains("database.kdbx"), "body was: {body}");
}

#[tokio::test]
async fn propfind_on_database_kdbx_returns_multistatus_with_content_length_and_last_modified_properties(
) {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Alice Props", "Alice Entry");
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

    let response = authed_propfind(
        &client,
        "alice-user",
        "alice-pass",
        &format!("{}/dav/alice/database.kdbx", server.base_url),
        "0",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status().as_u16(), 207);

    let body = response.text().await.unwrap();
    assert!(body.contains("getcontentlength"), "body was: {body}");
    assert!(body.contains("getlastmodified"), "body was: {body}");
}

#[tokio::test]
async fn propfind_on_root_collection_lists_exactly_one_entry_database_kdbx() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed_propfind(
        &client,
        "alice-user",
        "alice-pass",
        &format!("{}/dav/alice/", server.base_url),
        "1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status().as_u16(), 207);

    let body = response.text().await.unwrap();
    let href = "/dav/alice/database.kdbx";
    assert_eq!(body.match_indices(href).count(), 1, "body was: {body}");
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
