//! Step 6 — KDBX virtual-file construction.
//!
//! Converts between the git-stored [`StorageDatabase`] and raw KDBX 4.x bytes
//! using keepass-nd's `Database::save` / `Database::open`.

use std::path::Path;

use crate::storage::{
    convert::{db_to_storage, storage_to_db},
    types::StorageDatabase,
};
use eyre::{Context, Result};
use keepass::{Database, DatabaseKey};

pub trait KdbxCredentials {
    fn password(&self) -> Option<&str>;
    fn keyfile(&self) -> Option<&Path>;
}

/// Build a [`DatabaseKey`] from the configured credentials.
pub fn make_key(creds: &impl KdbxCredentials) -> Result<DatabaseKey> {
    let mut key = DatabaseKey::new();
    if let Some(password) = creds.password() {
        key = key.with_password(password);
    }
    if let Some(keyfile_path) = creds.keyfile() {
        let mut f = std::fs::File::open(keyfile_path).wrap_err("failed to open keyfile")?;
        key = key
            .with_keyfile(&mut f)
            .wrap_err("failed to load keyfile")?;
    }
    Ok(key)
}

/// Serialize `storage` to KDBX 4.1 bytes.
///
/// Blocking — call inside `tokio::task::spawn_blocking`.
pub fn build_kdbx_sync(storage: &StorageDatabase, creds: &impl KdbxCredentials) -> Result<Vec<u8>> {
    let db = storage_to_db(storage).wrap_err("failed to reconstruct database")?;
    let key = make_key(creds)?;
    let mut out = Vec::new();
    db.save(&mut out, key)
        .map_err(|e| eyre::eyre!("failed to save KDBX: {e}"))?;
    Ok(out)
}

/// Decrypt `bytes` and convert the result to a [`StorageDatabase`].
///
/// Blocking — call inside `tokio::task::spawn_blocking`.
pub fn parse_kdbx_sync(bytes: &[u8], creds: &impl KdbxCredentials) -> Result<StorageDatabase> {
    let key = make_key(creds)?;
    let db = Database::open(&mut &bytes[..], key)
        .map_err(|e| eyre::eyre!("failed to open KDBX: {e:?}"))?;
    db_to_storage(&db).wrap_err("failed to convert database to storage")
}
