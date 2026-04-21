use eyre::Result;
use kdbx_git_common::kdbx::KdbxCredentials;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level server configuration, loaded from a TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Path to the bare git repository used for storage.
    pub git_store: PathBuf,
    /// Optional path to the JSON sync state file used for push subscriptions
    /// and the server VAPID keypair.
    ///
    /// Defaults to `sync-state.json` next to `git_store`.
    pub sync_state_path: Option<PathBuf>,
    /// HTTP bind address, e.g. `"0.0.0.0:8080"`.
    pub bind_addr: String,
    /// Credentials used to open and save the KDBX database.
    pub database: DatabaseCredentials,
    /// KeeGate API settings.
    #[serde(default)]
    pub keegate_api: KeeGateApiConfig,
    /// Web UI settings.
    #[serde(default)]
    pub web_ui: WebUiConfig,
    /// One entry per WebDAV client.
    pub clients: Vec<ClientConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeeGateApiConfig {
    #[serde(default = "default_keegate_api_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebUiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_web_ui_base_path")]
    pub base_path: String,
    #[serde(default = "default_web_ui_session_ttl_hours")]
    pub session_ttl_hours: u64,
    #[serde(default)]
    pub admin_users: Vec<WebUiAdminUser>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebUiAdminUser {
    pub username: String,
    pub password: String,
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

    pub fn resolved_sync_state_path(&self) -> PathBuf {
        self.sync_state_path.clone().unwrap_or_else(|| {
            self.git_store
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("sync-state.json")
        })
    }
}

impl Default for KeeGateApiConfig {
    fn default() -> Self {
        Self {
            enabled: default_keegate_api_enabled(),
        }
    }
}

impl Default for WebUiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_path: default_web_ui_base_path(),
            session_ttl_hours: default_web_ui_session_ttl_hours(),
            admin_users: Vec::new(),
        }
    }
}

fn default_keegate_api_enabled() -> bool {
    true
}

fn default_web_ui_base_path() -> String {
    "/ui".to_string()
}

fn default_web_ui_session_ttl_hours() -> u64 {
    8
}

impl KdbxCredentials for DatabaseCredentials {
    fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    fn keyfile(&self) -> Option<&Path> {
        self.keyfile.as_deref()
    }
}
