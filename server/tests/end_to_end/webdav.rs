use super::*;

#[tokio::test]
async fn init_imports_main_and_git_history_is_readable() {
    let tempdir = TempDir::new().unwrap();
    let source_db = sample_db("Imported DB", "Imported Entry");
    let source_path = tempdir.path().join("source.kdbx");
    let config = test_config(tempdir.path());
    let config_path = tempdir.path().join("config.toml");

    write_source_kdbx(&source_path, &source_db, &config.database);
    write_config(&config_path, &config);

    init_from_config_path(&config_path, &source_path)
        .await
        .unwrap();

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
        "bob",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_bytes = build_kdbx_bytes(&alice_db, &config.database);
    let alice_put = authed(
        &client,
        "alice",
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
        "bob",
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
        "bob",
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
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
        "alice",
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
    let config = test_config(tempdir.path());
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
        "alice",
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let bob_db = sample_db("Shared DB", "Bob Entry");
    let put = authed(
        &client,
        "bob",
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_put = authed(
        &client,
        "alice",
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
        "bob",
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
        "alice",
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
        "bob",
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
    let config = test_config(tempdir.path());
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Alice Props", "Alice Entry");
    let put = authed(
        &client,
        "alice",
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed_propfind(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let malformed = authed(
        &client,
        "alice",
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
            password: Some(format!("{MASTER_PASSWORD}-wrong")),
            keyfile: None,
        },
    );
    let wrong_password = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Alice Write", "Alice Entry");
    let put = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let alice_db = sample_db("Alice Stable", "Alice Entry");
    let first_put = authed(
        &client,
        "alice",
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
        "alice",
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Alice Version 1", "Alice Entry");
    let first_put = authed(
        &client,
        "alice",
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
        "alice",
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
    let config = test_config(tempdir.path());
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let mut alice_db = sample_db("Shared DB", "Alice Entry");
    let alice_put = authed(
        &client,
        "alice",
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
        "bob",
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
        "bob",
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
        "alice",
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
async fn empty_put_body_is_rejected_without_committing() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let put = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
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
        "alice",
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
        "alice",
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
    let config = test_config(tempdir.path());
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
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
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
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
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
