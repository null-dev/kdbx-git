use eyre::Result;
use kdbx_git_common::kdbx::KdbxCredentials;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level server configuration, loaded from a TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Path to the bare git repository used for storage.
    pub git_store: PathBuf,
    /// HTTP bind address, e.g. `"0.0.0.0:8080"`.
    pub bind_addr: String,
    /// Credentials used to open and save the KDBX database.
    pub database: DatabaseCredentials,
    /// One entry per WebDAV client.
    pub clients: Vec<ClientConfig>,
}

/// Credentials for opening/saving the server-managed KDBX database.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseCredentials {
    /// Master password (optional if a key file is provided).
    pub password: Option<String>,
    /// Path to a KeePass key file (optional).
    pub keyfile: Option<PathBuf>,
}

/// Per-client configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClientConfig {
    /// Unique client identifier; used as the git branch name and HTTP username.
    pub id: String,
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

impl KdbxCredentials for DatabaseCredentials {
    fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    fn keyfile(&self) -> Option<&Path> {
        self.keyfile.as_deref()
    }
}
