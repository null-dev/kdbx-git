use std::path::Path;

use crate::config::Config;
use color_eyre::eyre::{Context, Result};
use kdbx_git_common::{kdbx::parse_kdbx_sync, store::GitStore};
use tokio::task::spawn_blocking;
use tracing::info;

pub async fn init_from_config_path(config_path: &Path, source_path: &Path) -> Result<()> {
    let config = Config::from_file(config_path)?;
    init_from_config(&config, source_path).await
}

pub async fn init_from_config(config: &Config, source_path: &Path) -> Result<()> {
    let bytes = std::fs::read(source_path).wrap_err_with(|| {
        format!(
            "failed to read source KDBX file at {}",
            source_path.display()
        )
    })?;

    let creds = config.database.clone();
    let storage = spawn_blocking(move || parse_kdbx_sync(&bytes, &creds))
        .await
        .wrap_err("database import task panicked")??;

    let store = GitStore::open_or_init(&config.git_store)?;
    let commit_id = store
        .bootstrap_main(storage, format!("import {}", source_path.display()))
        .await?;

    info!(
        "Imported '{}' into '{}' as main @ {}",
        source_path.display(),
        config.git_store.display(),
        commit_id
    );

    Ok(())
}
