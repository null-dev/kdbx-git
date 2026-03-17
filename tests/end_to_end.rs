mod common;

use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use common::{
    add_entry, build_kdbx_bytes, entry_titles, parse_kdbx_bytes, sample_db, test_config,
    write_config, write_source_kdbx, TestServer, MASTER_PASSWORD,
};
use futures_util::StreamExt;
use kdbx_git::{
    init::init_from_config_path,
    storage::types::{
        StorageCustomDataItem, StorageCustomDataValue, StorageEntry, StorageGroup, StorageTimes,
        StorageValue,
    },
    store::{merge_databases, GitStore},
};
use reqwest::{header, Client, StatusCode};
use tempfile::TempDir;
use tokio::{sync::mpsc, time::sleep};

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

fn make_group(uuid: &str, name: &str) -> StorageGroup {
    StorageGroup {
        uuid: uuid.into(),
        name: name.into(),
        notes: None,
        icon_id: None,
        custom_icon: None,
        groups: vec![],
        entries: vec![],
        times: StorageTimes {
            creation: Some("2024-01-01T00:00:00".into()),
            last_modification: Some("2024-01-01T00:00:01".into()),
            last_access: None,
            expiry: None,
            location_changed: Some("2024-01-01T00:00:01".into()),
            expires: Some(false),
            usage_count: Some(0),
        },
        custom_data: BTreeMap::new(),
        is_expanded: true,
        default_autotype_sequence: None,
        enable_autotype: None,
        enable_searching: None,
        last_top_visible_entry: None,
        tags: vec![],
        previous_parent_group: None,
    }
}

fn make_entry(uuid: &str, title: &str, username: &str, password: &str) -> StorageEntry {
    let mut fields = BTreeMap::new();
    fields.insert(
        "Title".into(),
        StorageValue {
            value: title.into(),
            protected: false,
        },
    );
    fields.insert(
        "UserName".into(),
        StorageValue {
            value: username.into(),
            protected: false,
        },
    );
    fields.insert(
        "Password".into(),
        StorageValue {
            value: password.into(),
            protected: true,
        },
    );

    StorageEntry {
        uuid: uuid.into(),
        fields,
        autotype: None,
        tags: vec![],
        times: StorageTimes {
            creation: Some("2024-01-01T00:00:00".into()),
            last_modification: Some("2024-01-01T00:00:01".into()),
            last_access: None,
            expiry: None,
            location_changed: None,
            expires: Some(false),
            usage_count: Some(0),
        },
        custom_data: BTreeMap::new(),
        icon_id: None,
        custom_icon: None,
        foreground_color: None,
        background_color: None,
        override_url: None,
        quality_check: None,
        previous_parent_group: None,
        attachments: BTreeMap::new(),
        history: vec![],
    }
}

fn find_group_mut<'a>(group: &'a mut StorageGroup, uuid: &str) -> Option<&'a mut StorageGroup> {
    if group.uuid == uuid {
        return Some(group);
    }

    for child in &mut group.groups {
        if let Some(found) = find_group_mut(child, uuid) {
            return Some(found);
        }
    }

    None
}

fn find_entry<'a>(group: &'a StorageGroup, uuid: &str) -> Option<&'a StorageEntry> {
    for entry in &group.entries {
        if entry.uuid == uuid {
            return Some(entry);
        }
    }

    for child in &group.groups {
        if let Some(found) = find_entry(child, uuid) {
            return Some(found);
        }
    }

    None
}

fn find_entry_mut<'a>(group: &'a mut StorageGroup, uuid: &str) -> Option<&'a mut StorageEntry> {
    for entry in &mut group.entries {
        if entry.uuid == uuid {
            return Some(entry);
        }
    }

    for child in &mut group.groups {
        if let Some(found) = find_entry_mut(child, uuid) {
            return Some(found);
        }
    }

    None
}

fn remove_entry(group: &mut StorageGroup, uuid: &str) -> Option<StorageEntry> {
    if let Some(idx) = group.entries.iter().position(|entry| entry.uuid == uuid) {
        return Some(group.entries.remove(idx));
    }

    for child in &mut group.groups {
        if let Some(removed) = remove_entry(child, uuid) {
            return Some(removed);
        }
    }

    None
}

fn history_titles(entry: &StorageEntry) -> Vec<String> {
    entry
        .history
        .iter()
        .filter_map(|history_entry| history_entry.fields.get("Title"))
        .map(|value| value.value.clone())
        .collect()
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

fn git_rev_count(git_dir: &Path, rev: &str) -> usize {
    git(git_dir, &["rev-list", "--count", rev], None)
        .parse()
        .unwrap()
}

fn git_log_subjects(git_dir: &Path, rev: &str) -> Vec<String> {
    let output = git(git_dir, &["log", "--format=%s", rev], None);
    if output.is_empty() {
        vec![]
    } else {
        output.lines().map(|line| line.to_string()).collect()
    }
}

async fn wait_for<F, Fut>(timeout_after: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout_after;
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

fn spawn_sse_listener(
    client: Client,
    username: &str,
    password: &str,
    url: String,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedReceiver<(String, String)>,
) {
    let username = username.to_string();
    let password = password.to_string();
    let (tx, rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let response = client
            .get(&url)
            .basic_auth(&username, Some(&password))
            .header(header::ACCEPT, "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.unwrap();
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            buffer = buffer.replace("\r\n", "\n");

            while let Some(idx) = buffer.find("\n\n") {
                let frame = buffer[..idx].to_string();
                buffer = buffer[idx + 2..].to_string();

                let mut event_name = None;
                let mut data_lines = Vec::new();
                for line in frame.lines() {
                    if let Some(name) = line.strip_prefix("event:") {
                        event_name = Some(name.trim().to_string());
                    } else if let Some(data) = line.strip_prefix("data:") {
                        data_lines.push(data.trim().to_string());
                    }
                }

                if let Some(name) = event_name {
                    let _ = tx.send((name, data_lines.join("\n")));
                }
            }
        }
    });

    (handle, rx)
}

struct SyncMergeHttpResult {
    commit_id: String,
    expected_tip: String,
    body: Vec<u8>,
}

async fn post_sync_merge_from_main(
    client: &Client,
    username: &str,
    password: &str,
    base_url: &str,
    client_id: &str,
) -> reqwest::Response {
    authed(
        client,
        username,
        password,
        reqwest::Method::POST,
        &format!("{}/sync/{client_id}/merge-from-main", base_url),
    )
    .send()
    .await
    .unwrap()
}

async fn post_sync_merge_from_main_ok(
    client: &Client,
    username: &str,
    password: &str,
    base_url: &str,
    client_id: &str,
) -> SyncMergeHttpResult {
    let response = post_sync_merge_from_main(client, username, password, base_url, client_id).await;
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
        .unwrap()
        .to_string();
    let body = response.bytes().await.unwrap().to_vec();

    SyncMergeHttpResult {
        commit_id,
        expected_tip,
        body,
    }
}

async fn post_sync_promote_merge(
    client: &Client,
    username: &str,
    password: &str,
    base_url: &str,
    client_id: &str,
    commit_id: &str,
    expected_tip: &str,
) -> reqwest::Response {
    authed(
        client,
        username,
        password,
        reqwest::Method::POST,
        &format!(
            "{}/sync/{}/promote-merge/{}?expected-tip={}",
            base_url, client_id, commit_id, expected_tip
        ),
    )
    .send()
    .await
    .unwrap()
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
async fn get_on_directory_path_returns_autoindex_listing_containing_database_kdbx() {
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
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );

    let body = response.text().await.unwrap();
    assert!(body.contains("database.kdbx"), "body was: {body}");
    assert!(body.contains("<html"), "body was: {body}");
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
async fn valid_put_creates_client_branch_and_main_with_success_status_and_commit_message() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Alice Write", "Alice Entry");
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

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip, main_tip, "main should advance to alice's write");

    let alice_branch = store.read_branch("alice".into()).await.unwrap().unwrap();
    let main_branch = store.read_branch("main".into()).await.unwrap().unwrap();
    assert_storage_eq(&alice_branch, &alice_db);
    assert_storage_eq(&main_branch, &alice_db);

    let subjects = git_log_subjects(&config.git_store, "main");
    assert_eq!(subjects[0], "write from client 'alice'");
    assert!(
        subjects[0].contains("alice"),
        "commit message should reference the client id: {subjects:?}"
    );
}

#[tokio::test]
async fn second_identical_put_is_a_noop_and_does_not_fire_sse_event() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Alice Stable", "Alice Entry");
    let first_put = authed(
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
    assert!(first_put.status().is_success());

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_before = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    let main_rev_count_before = git_rev_count(&config.git_store, "main");

    let (sse_handle, mut sse_events) = spawn_sse_listener(
        client.clone(),
        "alice-user",
        "alice-pass",
        format!("{}/sync/alice/events", server.base_url),
    );

    let ready = tokio::time::timeout(Duration::from_secs(5), sse_events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready.0, "ready");

    let second_put = authed(
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
    assert!(second_put.status().is_success());

    let alice_tip_after = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_after = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip_after, alice_tip_before);
    assert_eq!(main_tip_after, main_tip_before);
    assert_eq!(
        git_rev_count(&config.git_store, "main"),
        main_rev_count_before
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(750), sse_events.recv())
            .await
            .is_err(),
        "identical PUT should not emit an SSE branch-updated event"
    );

    sse_handle.abort();
}

#[tokio::test]
async fn second_changed_put_creates_new_commit_and_updates_main() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Alice Version 1", "Alice Entry");
    let first_put = authed(
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
    assert!(first_put.status().is_success());

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_before = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    let main_rev_count_before = git_rev_count(&config.git_store, "main");

    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000014",
        "Alice Changed Entry",
        "alice",
        "updated-secret",
    );

    let second_put = authed(
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
    assert!(second_put.status().is_success());

    let alice_tip_after = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_after = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    assert_ne!(alice_tip_after, alice_tip_before);
    assert_ne!(main_tip_after, main_tip_before);
    assert_eq!(alice_tip_after, main_tip_after);
    assert_eq!(
        git_rev_count(&config.git_store, "main"),
        main_rev_count_before + 1
    );

    let main_branch = store.read_branch("main".into()).await.unwrap().unwrap();
    let titles = entry_titles(&main_branch);
    assert!(titles.contains(&"Alice Entry".to_string()));
    assert!(titles.contains(&"Alice Changed Entry".to_string()));
}

#[tokio::test]
async fn put_advances_main_only_when_client_merge_succeeds() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let seed_main = sample_db("Seed Main", "Seed Entry");
    store
        .commit_to_branch("main".into(), seed_main, "seed main".into())
        .await
        .unwrap();
    store.ensure_client_branch("alice".into()).await.unwrap();

    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let clean_main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    replace_main_with_corrupt_commit(&config.git_store, &clean_main_tip.to_hex().to_string());
    let corrupt_main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();

    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let alice_db = sample_db("Alice After Corruption", "Alice Entry");

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
        "client branch write should still succeed even if merge to main fails"
    );

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_tip_after = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_after = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    assert_ne!(alice_tip_after, alice_tip_before);
    assert_eq!(
        main_tip_after, corrupt_main_tip,
        "main should not advance when the merge fails"
    );

    let alice_branch = store.read_branch("alice".into()).await.unwrap().unwrap();
    assert_storage_eq(&alice_branch, &alice_db);
}

#[tokio::test]
async fn put_to_branch_behind_main_commits_client_branch_and_merges_into_main() {
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
    let mut bob_db = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000015",
        "Bob Entry",
        "bob",
        "bob-secret",
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

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_before = store.branch_tip_id("main".into()).await.unwrap().unwrap();

    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000016",
        "Alice Catch-up Entry",
        "alice",
        "alice-secret",
    );
    let stale_alice_put = authed(
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
    assert!(stale_alice_put.status().is_success());

    let alice_tip_after = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    let main_tip_after = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    assert_ne!(alice_tip_after, alice_tip_before);
    assert_ne!(main_tip_after, main_tip_before);

    let alice_branch = store.read_branch("alice".into()).await.unwrap().unwrap();
    let alice_titles = entry_titles(&alice_branch);
    assert!(alice_titles.contains(&"Alice Entry".to_string()));
    assert!(alice_titles.contains(&"Alice Catch-up Entry".to_string()));
    assert!(
        !alice_titles.contains(&"Bob Entry".to_string()),
        "alice's own branch should contain her write, not an eager fan-out from main"
    );

    let main_branch = store.read_branch("main".into()).await.unwrap().unwrap();
    let main_titles = entry_titles(&main_branch);
    assert!(main_titles.contains(&"Alice Entry".to_string()));
    assert!(main_titles.contains(&"Alice Catch-up Entry".to_string()));
    assert!(main_titles.contains(&"Bob Entry".to_string()));
    assert_eq!(
        git_log_subjects(&config.git_store, "main")[0],
        "merge 'alice' into 'main'"
    );
}

#[tokio::test]
async fn concurrent_puts_from_two_clients_are_serialized_in_main_history() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let base_db = sample_db("Shared Base", "Base Entry");
    let seed_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&base_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(seed_put.status().is_success());

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

    let mut alice_db = base_db.clone();
    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000017",
        "Alice Concurrent Entry",
        "alice",
        "alice-pass-2",
    );
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000018",
        "Bob Concurrent Entry",
        "bob",
        "bob-pass-2",
    );

    let alice_request = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_db, &config.database));
    let bob_request = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&bob_db, &config.database));

    let (alice_put, bob_put) = tokio::join!(alice_request.send(), bob_request.send());
    let alice_put = alice_put.unwrap();
    let bob_put = bob_put.unwrap();
    assert!(alice_put.status().is_success());
    assert!(bob_put.status().is_success());

    wait_for(Duration::from_secs(5), || {
        let store_path = config.git_store.clone();
        async move {
            let subjects = git_log_subjects(&store_path, "main");
            subjects.iter().any(|subject| subject.contains("alice"))
                && subjects.iter().any(|subject| subject.contains("bob"))
        }
    })
    .await;

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let main_branch = store.read_branch("main".into()).await.unwrap().unwrap();
    let titles = entry_titles(&main_branch);
    assert!(titles.contains(&"Alice Concurrent Entry".to_string()));
    assert!(titles.contains(&"Bob Concurrent Entry".to_string()));
}

#[tokio::test]
async fn empty_put_body_is_rejected_without_committing() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(Vec::new())
    .send()
    .await
    .unwrap();
    assert!(
        matches!(
            put.status(),
            StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
        ),
        "unexpected status for empty upload: {}",
        put.status()
    );

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());
    assert!(store.branch_tip_id("main".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn put_followed_immediately_by_get_returns_newly_written_content() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Write Then Read", "Alice Entry");
    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000019",
        "Write Then Read Entry",
        "alice",
        "read-after-write",
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
    assert!(put.status().is_success());

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
async fn nested_groups_survive_put_get_round_trip() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Nested Group DB", "Root Entry");
    alice_db.root.groups.push(make_group(
        "00000000-0000-0000-0000-000000000100",
        "Personal",
    ));
    let personal_group =
        find_group_mut(&mut alice_db.root, "00000000-0000-0000-0000-000000000100").unwrap();
    personal_group
        .groups
        .push(make_group("00000000-0000-0000-0000-000000000101", "Email"));
    let email_group =
        find_group_mut(personal_group, "00000000-0000-0000-0000-000000000101").unwrap();
    email_group.entries.push(make_entry(
        "00000000-0000-0000-0000-000000000102",
        "Nested Mail Entry",
        "nested-user",
        "nested-pass",
    ));

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
async fn custom_entry_fields_survive_put_get_round_trip() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Custom Fields DB", "Fielded Entry");
    let entry = find_entry_mut(&mut alice_db.root, "00000000-0000-0000-0000-000000000010").unwrap();
    entry.fields.insert(
        "URL".into(),
        StorageValue {
            value: "https://example.com/login".into(),
            protected: false,
        },
    );
    entry.fields.insert(
        "Notes".into(),
        StorageValue {
            value: "line one\nline two".into(),
            protected: false,
        },
    );
    entry.fields.insert(
        "API Key".into(),
        StorageValue {
            value: "abc123-secret".into(),
            protected: true,
        },
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
    assert!(put.status().is_success());

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
async fn deleted_entries_do_not_reappear_after_merge_on_read() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let deleted_uuid = "00000000-0000-0000-0000-000000000103";
    let mut initial_db = sample_db("Deletion DB", "Shared Entry");
    initial_db
        .root
        .entries
        .push(make_entry(deleted_uuid, "Delete Me", "alice", "delete-me"));

    let initial_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&initial_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(initial_put.status().is_success());

    let bob_initial_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_initial_get.status(), StatusCode::OK);
    let bob_initial = parse_kdbx_bytes(&bob_initial_get.bytes().await.unwrap(), &config.database);
    assert!(find_entry(&bob_initial.root, deleted_uuid).is_some());

    let mut alice_deleted_db = initial_db.clone();
    remove_entry(&mut alice_deleted_db.root, deleted_uuid)
        .expect("entry should exist before deletion");
    alice_deleted_db
        .deleted_objects
        .insert(deleted_uuid.into(), Some("2024-01-01T00:20:00".into()));

    let delete_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_deleted_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(delete_put.status().is_success());

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

    let fetched = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    assert!(find_entry(&fetched.root, deleted_uuid).is_none());
    assert_eq!(
        fetched.deleted_objects.get(deleted_uuid),
        Some(&Some("2024-01-01T00:20:00".into()))
    );
}

#[tokio::test]
async fn modified_entries_are_visible_to_other_clients_after_merge_on_read() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let base_db = sample_db("Modification DB", "Shared Entry");
    let initial_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&base_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(initial_put.status().is_success());

    let bob_initial_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_initial_get.status(), StatusCode::OK);

    let mut alice_updated_db = base_db.clone();
    let entry = find_entry_mut(
        &mut alice_updated_db.root,
        "00000000-0000-0000-0000-000000000010",
    )
    .unwrap();
    entry.fields.get_mut("Title").unwrap().value = "Updated Shared Entry".into();
    entry.fields.get_mut("UserName").unwrap().value = "alice-updated".into();
    entry.times.last_modification = Some("2024-01-01T00:10:00".into());

    let update_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&alice_updated_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(update_put.status().is_success());

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

    let fetched = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    let fetched_entry = find_entry(&fetched.root, "00000000-0000-0000-0000-000000000010").unwrap();
    assert_eq!(
        fetched_entry
            .fields
            .get("Title")
            .map(|value| value.value.as_str()),
        Some("Updated Shared Entry")
    );
    assert_eq!(
        fetched_entry
            .fields
            .get("UserName")
            .map(|value| value.value.as_str()),
        Some("alice-updated")
    );
}

#[tokio::test]
async fn conflicting_same_uuid_edits_merge_deterministically() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let base_db = sample_db("Conflict DB", "Shared Entry");
    let initial_put = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&base_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(initial_put.status().is_success());

    let bob_seed_get = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_seed_get.status(), StatusCode::OK);
    let mut bob_db = parse_kdbx_bytes(&bob_seed_get.bytes().await.unwrap(), &config.database);

    let mut alice_db = base_db.clone();
    let alice_entry =
        find_entry_mut(&mut alice_db.root, "00000000-0000-0000-0000-000000000010").unwrap();
    alice_entry.fields.get_mut("Title").unwrap().value = "Alice Edit".into();
    alice_entry.fields.get_mut("Password").unwrap().value = "alice-secret".into();
    alice_entry.times.last_modification = Some("2024-01-01T00:10:00".into());

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

    let bob_entry =
        find_entry_mut(&mut bob_db.root, "00000000-0000-0000-0000-000000000010").unwrap();
    bob_entry.fields.get_mut("Title").unwrap().value = "Bob Edit".into();
    bob_entry.fields.get_mut("Password").unwrap().value = "bob-secret".into();
    bob_entry.times.last_modification = Some("2024-01-01T00:20:00".into());

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

    let fetched = parse_kdbx_bytes(&alice_get.bytes().await.unwrap(), &config.database);
    let fetched_entry = find_entry(&fetched.root, "00000000-0000-0000-0000-000000000010").unwrap();
    assert_eq!(
        fetched_entry
            .fields
            .get("Title")
            .map(|value| value.value.as_str()),
        Some("Bob Edit")
    );
    assert_eq!(
        fetched_entry
            .fields
            .get("Password")
            .map(|value| value.value.as_str()),
        Some("bob-secret")
    );

    let history = history_titles(fetched_entry);
    assert!(
        history.contains(&"Alice Edit".to_string()),
        "expected merged history to retain alice's conflicting edit, got {history:?}"
    );

    let expected_after_bob_merge = merge_databases(&alice_db, &bob_db).unwrap();
    let expected_entry = find_entry(
        &expected_after_bob_merge.root,
        "00000000-0000-0000-0000-000000000010",
    )
    .unwrap();
    assert_eq!(
        fetched_entry
            .fields
            .get("Title")
            .map(|value| value.value.as_str()),
        expected_entry
            .fields
            .get("Title")
            .map(|value| value.value.as_str())
    );
    assert_eq!(
        history_titles(fetched_entry),
        history_titles(expected_entry)
    );
}

#[tokio::test]
async fn database_name_metadata_survives_put_get_round_trip() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Original Name", "Metadata Entry");
    alice_db.meta.database_name = Some("Renamed Through WebDAV".into());
    alice_db.meta.database_name_changed = Some("2024-01-02T03:04:05".into());
    alice_db.meta.custom_data.insert(
        "meta-note".into(),
        StorageCustomDataItem {
            value: Some(StorageCustomDataValue::String("metadata survives".into())),
            last_modification_time: Some("2024-01-02T03:04:05".into()),
        },
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
    assert!(put.status().is_success());

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

#[tokio::test]
async fn correct_credentials_do_not_grant_access_to_another_clients_dav_endpoint() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"kdbx-git\"")
    );
}

#[tokio::test]
async fn username_from_one_client_with_another_clients_password_is_rejected() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn credentials_are_case_sensitive() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice-user",
        "Alice-Pass",
        reqwest::Method::GET,
        &format!("{}/dav/alice/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_client_paths_return_unauthorized() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice-user",
        "alice-pass",
        reqwest::Method::GET,
        &format!("{}/dav/nobody/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"kdbx-git\"")
    );
}

#[tokio::test]
async fn sync_merge_from_main_returns_no_content_when_client_branch_already_contains_main() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

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

    let response = post_sync_merge_from_main(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().get("X-Merge-Commit-Id").is_none());
    assert!(response.headers().get("X-Expected-Branch-Tip").is_none());
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn sync_merge_from_main_returns_kdbx_and_headers_when_merge_is_needed() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let store = GitStore::open_or_init(&config.git_store).unwrap();

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

    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert!(!merge.commit_id.is_empty());
    assert_eq!(merge.expected_tip, alice_tip_before.to_hex().to_string());

    let parsed = parse_kdbx_bytes(&merge.body, &config.database);
    let titles = entry_titles(&parsed);
    assert!(titles.contains(&"Alice Entry".to_string()));
    assert!(titles.contains(&"Bob Entry".to_string()));
}

#[tokio::test]
async fn sync_merge_from_main_returns_no_content_when_main_does_not_exist() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = post_sync_merge_from_main(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn sync_merge_from_main_returns_main_content_and_none_expected_tip_for_new_client_branch() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let bob_db = sample_db("Bob DB", "Bob Entry");
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

    let main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();
    assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_eq!(merge.expected_tip, "none");
    assert_eq!(merge.commit_id, main_tip.to_hex().to_string());
    let parsed = parse_kdbx_bytes(&merge.body, &config.database);
    assert_eq!(entry_titles(&parsed), vec!["Bob Entry".to_string()]);
    assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn sync_promote_merge_with_none_expected_tip_creates_client_branch() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let bob_db = sample_db("Bob DB", "Bob Entry");
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

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_eq!(merge.expected_tip, "none");

    let promote = post_sync_promote_merge(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
        &merge.commit_id,
        &merge.expected_tip,
    )
    .await;
    assert_eq!(promote.status(), StatusCode::OK);

    let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip.to_hex().to_string(), merge.commit_id);

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
    let parsed = parse_kdbx_bytes(&alice_get.bytes().await.unwrap(), &config.database);
    assert_eq!(entry_titles(&parsed), vec!["Bob Entry".to_string()]);
}

#[tokio::test]
async fn sync_promote_merge_advances_client_branch_when_expected_tip_matches() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();
    let store = GitStore::open_or_init(&config.git_store).unwrap();

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
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000021",
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

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_ne!(merge.expected_tip, "none");

    let promote = post_sync_promote_merge(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
        &merge.commit_id,
        &merge.expected_tip,
    )
    .await;
    assert_eq!(promote.status(), StatusCode::OK);

    let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
    assert_eq!(alice_tip.to_hex().to_string(), merge.commit_id);

    let reconcile = post_sync_merge_from_main(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_eq!(reconcile.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn sync_promote_merge_returns_conflict_when_branch_tip_changes() {
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
    let mut bob_db = parse_kdbx_bytes(&bob_get.bytes().await.unwrap(), &config.database);
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000022",
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

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;

    add_entry(
        &mut alice_db,
        "00000000-0000-0000-0000-000000000023",
        "Alice Branch Entry",
        "alice",
        "branchpass",
    );
    let alice_rewrite = authed(
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
    assert!(alice_rewrite.status().is_success());

    let promote = post_sync_promote_merge(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
        &merge.commit_id,
        &merge.expected_tip,
    )
    .await;
    assert_eq!(promote.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn sync_promote_merge_rejects_bad_commit_hex() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = post_sync_promote_merge(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
        "not-hex",
        "none",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sync_promote_merge_rejects_bad_expected_tip_hex() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let bob_db = sample_db("Bob DB", "Bob Entry");
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

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;

    let response = post_sync_promote_merge(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
        &merge.commit_id,
        "not-hex",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sync_events_stream_emits_branch_updated_when_main_advances() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let (sse_handle, mut sse_events) = spawn_sse_listener(
        client.clone(),
        "alice-user",
        "alice-pass",
        format!("{}/sync/alice/events", server.base_url),
    );

    let ready = tokio::time::timeout(Duration::from_secs(5), sse_events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready.0, "ready");

    let bob_db = sample_db("Bob DB", "Bob Entry");
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

    let update = tokio::time::timeout(Duration::from_secs(5), sse_events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(update.0, "branch-updated");
    assert!(update.1.parse::<u64>().unwrap() >= 1);

    sse_handle.abort();
}

#[tokio::test]
async fn sync_events_stream_sends_ready_immediately() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let (sse_handle, mut sse_events) = spawn_sse_listener(
        client,
        "alice-user",
        "alice-pass",
        format!("{}/sync/alice/events", server.base_url),
    );

    let ready = tokio::time::timeout(Duration::from_secs(5), sse_events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready.0, "ready");
    assert_eq!(ready.1, "0");

    sse_handle.abort();
}
