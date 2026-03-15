use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{bail, Context, Result};
use gix::ObjectId;
use tokio::{fs, task::spawn_blocking, time::sleep};
use tracing::{info, warn};

use crate::{
    config::Config,
    kdbx::{build_kdbx_sync, parse_kdbx_sync},
    storage::{
        format::{serialize, StorageFormat},
        types::StorageDatabase,
    },
    store::{merge_databases, GitStore},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLocalOptions {
    pub client_id: String,
    pub local_path: PathBuf,
    pub once: bool,
    pub interval_secs: u64,
}

#[derive(Debug, Clone)]
struct SyncSnapshot {
    remote_tip: Option<ObjectId>,
    remote_storage: Option<StorageDatabase>,
    remote_repr: Option<String>,
    local_storage: Option<StorageDatabase>,
    local_repr: Option<String>,
    local_sig: Option<u64>,
}

pub async fn sync_local_from_config_path(
    config_path: &Path,
    options: SyncLocalOptions,
) -> Result<()> {
    let config = Config::from_file(config_path)?;
    let store = GitStore::open_or_init(&config.git_store)?;
    sync_local(config, store, options).await
}

pub async fn sync_local(config: Config, store: GitStore, options: SyncLocalOptions) -> Result<()> {
    if !config
        .clients
        .iter()
        .any(|client| client.id == options.client_id)
    {
        bail!("unknown client id '{}'", options.client_id);
    }

    let mut syncer = LocalSyncer::new(config, store, options);

    loop {
        syncer.reconcile_once().await?;
        if syncer.options.once {
            return Ok(());
        }
        sleep(Duration::from_secs(syncer.options.interval_secs)).await;
    }
}

struct LocalSyncer {
    config: Config,
    store: GitStore,
    options: SyncLocalOptions,
    last_remote_tip: Option<ObjectId>,
    last_local_sig: Option<u64>,
}

impl LocalSyncer {
    fn new(config: Config, store: GitStore, options: SyncLocalOptions) -> Self {
        Self {
            config,
            store,
            options,
            last_remote_tip: None,
            last_local_sig: None,
        }
    }

    async fn reconcile_once(&mut self) -> Result<()> {
        self.store
            .ensure_client_branch(self.options.client_id.clone())
            .await?;

        let snapshot = self.read_snapshot().await?;

        match (
            snapshot.remote_storage.as_ref(),
            snapshot.local_storage.as_ref(),
        ) {
            (None, None) => {
                info!(
                    "sync-local '{}': both branch and local file are empty",
                    self.options.client_id
                );
            }
            (Some(remote), None) => {
                info!(
                    "sync-local '{}': writing branch tip to {}",
                    self.options.client_id,
                    self.options.local_path.display()
                );
                self.write_local_file(remote).await?;
            }
            (None, Some(local)) => {
                info!(
                    "sync-local '{}': importing local file into branch",
                    self.options.client_id
                );
                self.push_local_to_branch(local.clone()).await?;
            }
            (Some(remote), Some(local)) => {
                if snapshot.remote_repr == snapshot.local_repr {
                    info!(
                        "sync-local '{}': branch and local file already match",
                        self.options.client_id
                    );
                } else {
                    let remote_changed = snapshot.remote_tip != self.last_remote_tip;
                    let local_changed = snapshot.local_sig != self.last_local_sig;

                    match (remote_changed, local_changed) {
                        (true, false) => {
                            info!(
                                "sync-local '{}': pulling branch changes into local file",
                                self.options.client_id
                            );
                            self.write_local_file(remote).await?;
                        }
                        (false, true) => {
                            info!(
                                "sync-local '{}': pushing local changes into branch",
                                self.options.client_id
                            );
                            self.push_local_to_branch(local.clone()).await?;
                        }
                        (true, true) | (false, false) => {
                            warn!(
                                "sync-local '{}': local file and branch diverged, merging both",
                                self.options.client_id
                            );
                            let merged = merge_databases(remote, local)?;
                            self.push_local_to_branch(merged.clone()).await?;
                            self.write_local_file(&merged).await?;
                        }
                    }
                }
            }
        }

        let refreshed = self.read_snapshot().await?;
        self.last_remote_tip = refreshed.remote_tip;
        self.last_local_sig = refreshed.local_sig;
        Ok(())
    }

    async fn read_snapshot(&self) -> Result<SyncSnapshot> {
        let remote_tip = self
            .store
            .branch_tip_id(self.options.client_id.clone())
            .await
            .wrap_err("failed to read client branch tip")?;

        let remote_storage = if remote_tip.is_some() {
            self.store
                .read_branch(self.options.client_id.clone())
                .await
                .wrap_err("failed to read client branch")?
        } else {
            None
        };

        let remote_repr = remote_storage
            .as_ref()
            .map(stable_storage_repr)
            .transpose()?;

        let local_bytes = match fs::read(&self.options.local_path).await {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).wrap_err_with(|| {
                    format!(
                        "failed to read local KDBX file {}",
                        self.options.local_path.display()
                    )
                })
            }
        };

        let local_sig = local_bytes.as_ref().map(|bytes| bytes_signature(bytes));
        let local_storage = match local_bytes {
            Some(bytes) => Some(self.parse_local_bytes(bytes).await?),
            None => None,
        };
        let local_repr = local_storage
            .as_ref()
            .map(stable_storage_repr)
            .transpose()?;

        Ok(SyncSnapshot {
            remote_tip,
            remote_storage,
            remote_repr,
            local_storage,
            local_repr,
            local_sig,
        })
    }

    async fn parse_local_bytes(&self, bytes: Vec<u8>) -> Result<StorageDatabase> {
        let creds = self.config.database.clone();
        spawn_blocking(move || parse_kdbx_sync(&bytes, &creds))
            .await
            .wrap_err("local KDBX parse task panicked")?
            .wrap_err_with(|| {
                format!(
                    "failed to parse local KDBX file {}",
                    self.options.local_path.display()
                )
            })
    }

    async fn write_local_file(&self, storage: &StorageDatabase) -> Result<()> {
        if let Some(parent) = self.options.local_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .await
                    .wrap_err_with(|| format!("failed to create directory {}", parent.display()))?;
            }
        }

        let creds = self.config.database.clone();
        let storage = storage.clone();
        let bytes = spawn_blocking(move || build_kdbx_sync(&storage, &creds))
            .await
            .wrap_err("local KDBX build task panicked")??;

        fs::write(&self.options.local_path, bytes)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to write local KDBX file {}",
                    self.options.local_path.display()
                )
            })?;
        Ok(())
    }

    async fn push_local_to_branch(&self, storage: StorageDatabase) -> Result<()> {
        let all_client_ids = self
            .config
            .clients
            .iter()
            .map(|client| client.id.clone())
            .collect();

        self.store
            .process_client_write(self.options.client_id.clone(), storage, all_client_ids)
            .await
            .wrap_err("failed to apply local sync write")
    }
}

fn stable_storage_repr(storage: &StorageDatabase) -> Result<String> {
    serialize(storage, StorageFormat::Json)
}

fn bytes_signature(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
