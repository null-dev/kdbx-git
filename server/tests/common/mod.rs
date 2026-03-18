#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

use color_eyre::eyre::Result;
use kdbx_git::{
    config::{ClientConfig, Config, DatabaseCredentials},
    kdbx::{build_kdbx_sync, parse_kdbx_sync},
    server::{serve_listener, AppState},
    storage::types::{
        StorageDatabase, StorageEntry, StorageGroup, StorageMeta, StorageTimes, StorageValue,
    },
    store::GitStore,
};
use kdbx_git_sync_local::config::Config as SyncLocalConfig;
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

pub const MASTER_PASSWORD: &str = "integration-test-password";

pub struct TestServer {
    _tempdir: TempDir,
    pub config: Config,
    pub base_url: String,
    handle: JoinHandle<Result<()>>,
}

impl TestServer {
    pub async fn start(config: Config, tempdir: TempDir) -> Result<Self> {
        let listener = TcpListener::bind(&config.bind_addr).await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let store = GitStore::open_or_init(&config.git_store)?;
        let state = AppState::new(config.clone(), store);
        let handle = tokio::spawn(async move { serve_listener(listener, state).await });

        Ok(Self {
            _tempdir: tempdir,
            config,
            base_url,
            handle,
        })
    }

    pub fn temp_root(&self) -> &Path {
        self._tempdir.path()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub fn test_credentials() -> DatabaseCredentials {
    DatabaseCredentials {
        password: Some(MASTER_PASSWORD.to_string()),
        keyfile: None,
    }
}

pub fn test_config(root: &Path) -> Config {
    Config {
        git_store: root.join("store.git"),
        bind_addr: "127.0.0.1:0".to_string(),
        database: test_credentials(),
        clients: vec![
            ClientConfig {
                id: "alice".into(),
                password: "alice-pass".into(),
            },
            ClientConfig {
                id: "bob".into(),
                password: "bob-pass".into(),
            },
            ClientConfig {
                id: "carol".into(),
                password: "carol-pass".into(),
            },
        ],
    }
}

pub fn sync_state_path(config: &Config) -> PathBuf {
    config
        .git_store
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("sync-state.json")
}

pub fn sample_db(name: &str, title: &str) -> StorageDatabase {
    let mut db = StorageDatabase {
        kdbx_config: Default::default(),
        meta: StorageMeta {
            generator: Some("kdbx-git-integration".into()),
            database_name: Some(name.into()),
            database_name_changed: None,
            database_description: None,
            database_description_changed: None,
            default_username: None,
            default_username_changed: None,
            maintenance_history_days: None,
            color: None,
            master_key_changed: None,
            master_key_change_rec: None,
            master_key_change_force: None,
            memory_protection: None,
            recyclebin_enabled: None,
            recyclebin_uuid: None,
            recyclebin_changed: None,
            entry_templates_group: None,
            entry_templates_group_changed: None,
            last_selected_group: None,
            last_top_visible_group: None,
            history_max_items: None,
            history_max_size: None,
            settings_changed: None,
            custom_data: BTreeMap::new(),
        },
        root: StorageGroup {
            uuid: "00000000-0000-0000-0000-000000000001".into(),
            name: "Root".into(),
            notes: None,
            icon_id: None,
            custom_icon: None,
            groups: vec![],
            entries: vec![],
            times: StorageTimes {
                creation: None,
                last_modification: None,
                last_access: None,
                expiry: None,
                location_changed: None,
                expires: None,
                usage_count: None,
            },
            custom_data: BTreeMap::new(),
            is_expanded: true,
            default_autotype_sequence: None,
            enable_autotype: None,
            enable_searching: None,
            last_top_visible_entry: None,
            tags: vec![],
            previous_parent_group: None,
        },
        deleted_objects: BTreeMap::new(),
    };

    add_entry(
        &mut db,
        "00000000-0000-0000-0000-000000000010",
        title,
        "demo-user",
        "hunter2",
    );

    db
}

pub fn add_entry(
    db: &mut StorageDatabase,
    uuid: &str,
    title: &str,
    username: &str,
    password: &str,
) {
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

    db.root.entries.push(StorageEntry {
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
    });
}

pub fn entry_titles(db: &StorageDatabase) -> Vec<String> {
    db.root
        .entries
        .iter()
        .filter_map(|entry| entry.fields.get("Title"))
        .map(|value| value.value.clone())
        .collect()
}

pub fn build_kdbx_bytes(db: &StorageDatabase, creds: &DatabaseCredentials) -> Vec<u8> {
    build_kdbx_sync(db, creds).expect("failed to build test KDBX")
}

pub fn parse_kdbx_bytes(bytes: &[u8], creds: &DatabaseCredentials) -> StorageDatabase {
    parse_kdbx_sync(bytes, creds).expect("failed to parse test KDBX")
}

pub fn write_source_kdbx(path: &Path, db: &StorageDatabase, creds: &DatabaseCredentials) {
    std::fs::write(path, build_kdbx_bytes(db, creds)).expect("failed to write test KDBX");
}

pub fn write_config(path: &Path, config: &Config) {
    let contents = toml::to_string_pretty(config).expect("failed to serialize config");
    std::fs::write(path, contents).expect("failed to write config file");
}

pub fn sync_local_config(server: &Config, client_id: &str, server_url: String) -> SyncLocalConfig {
    let client = server
        .clients
        .iter()
        .find(|client| client.id == client_id)
        .unwrap_or_else(|| panic!("missing test client configuration for '{client_id}'"));

    SyncLocalConfig {
        server_url,
        client_id: client.id.clone(),
        password: client.password.clone(),
    }
}

pub fn write_sync_local_config(path: &Path, config: &SyncLocalConfig) {
    let contents = toml::to_string_pretty(config).expect("failed to serialize sync-local config");
    std::fs::write(path, contents).expect("failed to write sync-local config");
}

pub fn sync_local_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();

    BIN.get_or_init(|| {
        if let Some(path) = std::env::var_os("CARGO_BIN_EXE_kdbx-git-sync-local") {
            return PathBuf::from(path);
        }

        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("server crate should have a workspace parent")
            .to_path_buf();
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join("target"));
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let binary_name = if cfg!(windows) {
            "kdbx-git-sync-local.exe"
        } else {
            "kdbx-git-sync-local"
        };
        let binary_path = target_dir.join(profile).join(binary_name);

        let status = Command::new("cargo")
            .args([
                "build",
                "-p",
                "kdbx-git-sync-local",
                "--bin",
                "kdbx-git-sync-local",
            ])
            .current_dir(&workspace_root)
            .status()
            .expect("failed to build sync-local binary");
        assert!(status.success(), "failed to build sync-local binary");

        binary_path
    })
    .as_path()
}
