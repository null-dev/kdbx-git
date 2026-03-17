use super::*;

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
