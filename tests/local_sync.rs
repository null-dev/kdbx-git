mod common;

use common::{
    add_entry, entry_titles, parse_kdbx_bytes, sample_db, test_config, write_source_kdbx,
};
use kdbx_git::{
    store::GitStore,
    sync::{sync_local, SyncLocalOptions},
};
use tempfile::TempDir;

#[tokio::test]
async fn sync_local_pushes_local_file_into_client_branch() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let local_path = tempdir.path().join("alice-local.kdbx");
    let local_db = sample_db("Local DB", "Local Entry");

    write_source_kdbx(&local_path, &local_db, &config.database);

    sync_local(
        config.clone(),
        store,
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            interval_secs: 1,
        },
    )
    .await
    .unwrap();

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice = store.read_branch("alice".into()).await.unwrap().unwrap();
    assert_eq!(entry_titles(&alice), vec!["Local Entry".to_string()]);
}

#[tokio::test]
async fn sync_local_pulls_branch_tip_into_missing_file() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let remote_db = sample_db("Remote DB", "Remote Entry");

    store
        .process_client_write(
            "alice".into(),
            remote_db,
            config
                .clients
                .iter()
                .map(|client| client.id.clone())
                .collect(),
        )
        .await
        .unwrap();

    let local_path = tempdir.path().join("alice-local.kdbx");
    sync_local(
        config.clone(),
        store,
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            interval_secs: 1,
        },
    )
    .await
    .unwrap();

    let bytes = tokio::fs::read(&local_path).await.unwrap();
    let parsed = parse_kdbx_bytes(&bytes, &config.database);
    assert_eq!(entry_titles(&parsed), vec!["Remote Entry".to_string()]);
}

#[tokio::test]
async fn sync_local_merges_divergent_local_and_remote_states() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path(), None);
    let store = GitStore::open_or_init(&config.git_store).unwrap();

    let mut remote_db = sample_db("Shared DB", "Shared Entry");
    add_entry(
        &mut remote_db,
        "00000000-0000-0000-0000-000000000020",
        "Remote Only",
        "remote",
        "remote-pass",
    );
    store
        .process_client_write(
            "alice".into(),
            remote_db,
            config
                .clients
                .iter()
                .map(|client| client.id.clone())
                .collect(),
        )
        .await
        .unwrap();

    let local_path = tempdir.path().join("alice-local.kdbx");
    let mut local_db = sample_db("Shared DB", "Shared Entry");
    add_entry(
        &mut local_db,
        "00000000-0000-0000-0000-000000000021",
        "Local Only",
        "local",
        "local-pass",
    );
    write_source_kdbx(&local_path, &local_db, &config.database);

    sync_local(
        config.clone(),
        store,
        SyncLocalOptions {
            client_id: "alice".into(),
            local_path: local_path.clone(),
            once: true,
            interval_secs: 1,
        },
    )
    .await
    .unwrap();

    let store = GitStore::open_or_init(&config.git_store).unwrap();
    let alice = store.read_branch("alice".into()).await.unwrap().unwrap();
    let titles = entry_titles(&alice);
    assert!(
        titles.contains(&"Shared Entry".to_string()),
        "titles were {titles:?}"
    );
    assert!(
        titles.contains(&"Remote Only".to_string()),
        "titles were {titles:?}"
    );
    assert!(
        titles.contains(&"Local Only".to_string()),
        "titles were {titles:?}"
    );

    let bytes = tokio::fs::read(&local_path).await.unwrap();
    let parsed = parse_kdbx_bytes(&bytes, &config.database);
    let local_titles = entry_titles(&parsed);
    assert!(
        local_titles.contains(&"Remote Only".to_string()),
        "titles were {local_titles:?}"
    );
    assert!(
        local_titles.contains(&"Local Only".to_string()),
        "titles were {local_titles:?}"
    );
}
