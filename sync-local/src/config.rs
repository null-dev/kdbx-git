use eyre::Result;
use serde::{Deserialize, Serialize};

/// Top-level sync-local client configuration, loaded from a TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Base URL of the running kdbx-git server.
    pub server_url: String,
    /// The client branch and sync endpoint to use.
    pub client_id: String,
    /// HTTP Basic Auth username for this client's WebDAV endpoint.
    pub username: String,
    /// HTTP Basic Auth password for this client's WebDAV endpoint.
    pub password: String,
}

impl Config {
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }
}
