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

#[path = "end_to_end/integrity.rs"]
mod integrity;
#[path = "end_to_end/keegate.rs"]
mod keegate;
#[path = "end_to_end/push.rs"]
mod push;
#[path = "end_to_end/sync.rs"]
mod sync;
#[path = "end_to_end/web_ui.rs"]
mod web_ui;
#[path = "end_to_end/webdav.rs"]
mod webdav;
