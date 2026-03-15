//! Step 6 — KDBX virtual-file construction.
//!
//! Converts between the git-stored [`StorageDatabase`] and raw KDBX 4.x bytes
//! using keepass-nd's `Database::save` / `Database::open`.

use crate::{
    config::DatabaseCredentials,
    storage::{
        convert::{db_to_storage, storage_to_db},
        types::StorageDatabase,
    },
};
use eyre::{Context, Result};
use keepass::{
    config::{
        CompressionConfig, DatabaseConfig, DatabaseVersion, InnerCipherConfig, KdfConfig,
        OuterCipherConfig,
    },
    Database, DatabaseKey,
};

/// Build a [`DatabaseKey`] from the configured credentials.
pub fn make_key(creds: &DatabaseCredentials) -> Result<DatabaseKey> {
    let mut key = DatabaseKey::new();
    if let Some(password) = &creds.password {
        key = key.with_password(password);
    }
    if let Some(keyfile_path) = &creds.keyfile {
        let mut f = std::fs::File::open(keyfile_path).wrap_err("failed to open keyfile")?;
        key = key.with_keyfile(&mut f).wrap_err("failed to load keyfile")?;
    }
    Ok(key)
}

/// Serialize `storage` to KDBX 4.1 bytes.
///
/// Blocking — call inside `tokio::task::spawn_blocking`.
pub fn build_kdbx_sync(
    storage: &StorageDatabase,
    creds: &DatabaseCredentials,
) -> Result<Vec<u8>> {
    let config = DatabaseConfig {
        version: DatabaseVersion::KDB4(1),
        outer_cipher_config: OuterCipherConfig::AES256,
        compression_config: CompressionConfig::GZip,
        inner_cipher_config: InnerCipherConfig::ChaCha20,
        kdf_config: KdfConfig::Aes { rounds: 600_000 },
        public_custom_data: None,
    };
    let db = storage_to_db(storage, config).wrap_err("failed to reconstruct database")?;
    let key = make_key(creds)?;
    let mut out = Vec::new();
    db.save(&mut out, key)
        .map_err(|e| eyre::eyre!("failed to save KDBX: {e}"))?;
    Ok(out)
}

/// Decrypt `bytes` and convert the result to a [`StorageDatabase`].
///
/// Blocking — call inside `tokio::task::spawn_blocking`.
pub fn parse_kdbx_sync(bytes: &[u8], creds: &DatabaseCredentials) -> Result<StorageDatabase> {
    let key = make_key(creds)?;
    let db = Database::open(&mut &bytes[..], key)
        .map_err(|e| eyre::eyre!("failed to open KDBX: {e:?}"))?;
    db_to_storage(&db).wrap_err("failed to convert database to storage")
}
