use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level sync-local client configuration, loaded from a TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Base URL of the running kdbx-git server.
    pub server_url: String,
    /// The client branch, sync endpoint, and HTTP username (same as the client ID).
    pub client_id: String,
    /// HTTP Basic Auth password for this client's WebDAV endpoint.
    pub password: String,
    /// Optional path to the JSON state file used for interrupt recovery.
    ///
    /// Defaults to `<local_path>.sync-state.json`.
    pub sync_state_path: Option<PathBuf>,
}

impl Config {
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn sync_state_path_for(&self, local_path: &Path) -> PathBuf {
        self.sync_state_path
            .clone()
            .unwrap_or_else(|| default_sync_state_path(local_path))
    }
}

pub fn default_sync_state_path(local_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sync-state.json", local_path.display()))
}
