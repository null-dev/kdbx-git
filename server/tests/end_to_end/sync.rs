use super::*;

#[tokio::test]
async fn concurrent_puts_from_two_clients_are_serialized_in_main_history() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
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
    let alice_titles = entry_titles(&parse_kdbx_bytes(
        &alice_get.bytes().await.unwrap(),
        &config.database,
    ));
    assert!(alice_titles.contains(&"Alice Concurrent Entry".to_string()));
    assert!(alice_titles.contains(&"Bob Concurrent Entry".to_string()));

    let bob_get_after = authed(
        &client,
        "bob-user",
        "bob-pass",
        reqwest::Method::GET,
        &format!("{}/dav/bob/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(bob_get_after.status(), StatusCode::OK);
    let bob_titles = entry_titles(&parse_kdbx_bytes(
        &bob_get_after.bytes().await.unwrap(),
        &config.database,
    ));
    assert!(bob_titles.contains(&"Alice Concurrent Entry".to_string()));
    assert!(bob_titles.contains(&"Bob Concurrent Entry".to_string()));
}

#[tokio::test]
async fn three_clients_writes_converge_for_all_clients() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
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
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000030",
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

    let carol_get = authed(
        &client,
        "carol-user",
        "carol-pass",
        reqwest::Method::GET,
        &format!("{}/dav/carol/database.kdbx", server.base_url),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(carol_get.status(), StatusCode::OK);
    let mut carol_db = parse_kdbx_bytes(&carol_get.bytes().await.unwrap(), &config.database);
    add_entry(
        &mut carol_db,
        "00000000-0000-0000-0000-000000000031",
        "Carol Entry",
        "carol",
        "carolpass",
    );
    let carol_put = authed(
        &client,
        "carol-user",
        "carol-pass",
        reqwest::Method::PUT,
        &format!("{}/dav/carol/database.kdbx", server.base_url),
    )
    .body(build_kdbx_bytes(&carol_db, &config.database))
    .send()
    .await
    .unwrap();
    assert!(carol_put.status().is_success());

    for (client_id, username, password) in [
        ("alice", "alice-user", "alice-pass"),
        ("bob", "bob-user", "bob-pass"),
        ("carol", "carol-user", "carol-pass"),
    ] {
        let get = authed(
            &client,
            username,
            password,
            reqwest::Method::GET,
            &format!("{}/dav/{client_id}/database.kdbx", server.base_url),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let titles = entry_titles(&parse_kdbx_bytes(
            &get.bytes().await.unwrap(),
            &config.database,
        ));
        assert!(titles.contains(&"Alice Entry".to_string()));
        assert!(titles.contains(&"Bob Entry".to_string()));
        assert!(titles.contains(&"Carol Entry".to_string()));
    }
}

#[tokio::test]
async fn sync_merge_from_main_returns_no_content_when_client_branch_already_contains_main() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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
    let config = test_config(tempdir.path());
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

#[tokio::test]
async fn get_after_sync_promote_still_returns_correct_merged_content() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
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
    add_entry(
        &mut bob_db,
        "00000000-0000-0000-0000-000000000040",
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
    let titles = entry_titles(&parse_kdbx_bytes(
        &alice_get.bytes().await.unwrap(),
        &config.database,
    ));
    assert!(titles.contains(&"Alice Entry".to_string()));
    assert!(titles.contains(&"Bob Entry".to_string()));
}

#[tokio::test]
async fn sync_merge_from_main_catches_up_branch_far_behind_main_in_one_call() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
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

    let alice_tip_before = store.branch_tip_id("alice".into()).await.unwrap().unwrap();

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

    for idx in 0..5 {
        add_entry(
            &mut bob_db,
            &format!("00000000-0000-0000-0000-00000000005{idx}"),
            &format!("Bob Entry {idx}"),
            "bob",
            &format!("bobpass-{idx}"),
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
    }

    let merge = post_sync_merge_from_main_ok(
        &client,
        "alice-user",
        "alice-pass",
        &server.base_url,
        "alice",
    )
    .await;
    assert_eq!(merge.expected_tip, alice_tip_before.to_hex().to_string());

    let parsed = parse_kdbx_bytes(&merge.body, &config.database);
    let titles = entry_titles(&parsed);
    assert!(titles.contains(&"Alice Entry".to_string()));
    for idx in 0..5 {
        assert!(
            titles.contains(&format!("Bob Entry {idx}")),
            "missing far-behind update Bob Entry {idx}: {titles:?}"
        );
    }
}
