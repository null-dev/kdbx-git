use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Credentials for opening/saving the KDBX database.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseCredentials {
    /// Path to an existing KDBX database used by `--init`.
    pub path: Option<PathBuf>,
    /// Master password (optional if a key file is provided).
    pub password: Option<String>,
    /// Path to a KeePass key file (optional).
    pub keyfile: Option<PathBuf>,
}
