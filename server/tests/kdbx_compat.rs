mod common;

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use common::{
    build_kdbx_bytes, sample_db, test_config, test_credentials, write_config, TestServer,
    MASTER_PASSWORD,
};
use kdbx_git::{
    init::init_from_config_path,
    storage::{
        convert::storage_to_db_with_config,
        types::{StorageCustomDataItem, StorageCustomDataValue, StorageDatabase},
    },
};
use keepass::{
    config::{
        CompressionConfig, DatabaseConfig, DatabaseVersion, InnerCipherConfig, KdfConfig,
        OuterCipherConfig,
    },
    Database,
};
use reqwest::{Client, StatusCode};
use tempfile::TempDir;

fn decrypted_xml(db: &kdbx_git::storage::types::StorageDatabase) -> String {
    let creds = test_credentials(None);
    let bytes = build_kdbx_bytes(db, &creds);
    let xml = Database::get_xml(
        &mut bytes.as_slice(),
        kdbx_git::kdbx::make_key(&creds).unwrap(),
    )
    .expect("failed to decrypt emitted KDBX");
    String::from_utf8(xml).expect("xml should be valid UTF-8")
}

#[test]
fn emitted_kdbx_omits_unset_optional_bool_fields() {
    let db = sample_db("Compat DB", "Compat Entry");
    let xml = decrypted_xml(&db);

    for invalid_tag in [
        "<RecycleBinEnabled/>",
        "<Expires/>",
        "<EnableAutoType/>",
        "<EnableSearching/>",
    ] {
        assert!(
            !xml.contains(invalid_tag),
            "unexpected invalid bool tag {invalid_tag} in emitted XML: {xml}"
        );
    }
}

#[test]
fn emitted_kdbx_keeps_explicit_group_bool_values() {
    let mut db = sample_db("Compat DB", "Compat Entry");
    db.root.enable_autotype = Some(false);
    db.root.enable_searching = Some(true);
    let xml = decrypted_xml(&db);

    assert!(
        xml.contains("<EnableAutoType>False</EnableAutoType>"),
        "{xml}"
    );
    assert!(
        xml.contains("<EnableSearching>True</EnableSearching>"),
        "{xml}"
    );
}

#[test]
fn emitted_kdbx_uses_keepassxc_compatible_timestamps_and_tag_names() {
    let mut db = sample_db("Compat DB", "Compat Entry");
    db.meta.default_username = Some("alice".into());
    db.meta.default_username_changed = Some("2024-01-01T00:00:00".into());
    db.meta.custom_data.insert(
        "example".into(),
        StorageCustomDataItem {
            value: Some(StorageCustomDataValue::String("value".into())),
            last_modification_time: None,
        },
    );
    db.root.times.creation = Some("2024-01-01T00:00:00".into());

    let xml = decrypted_xml(&db);

    assert!(
        xml.contains("<DefaultUserName>alice</DefaultUserName>"),
        "{xml}"
    );
    assert!(!xml.contains("<DefaultUsername>"), "{xml}");
    assert!(xml.contains("<DefaultUserNameChanged>"), "{xml}");
    assert!(!xml.contains("2024-01-01T00:00:00Z"), "{xml}");
    assert!(!xml.contains("<LastModificationTime/>"), "{xml}");
}

fn non_default_kdbx_config() -> DatabaseConfig {
    DatabaseConfig {
        version: DatabaseVersion::KDB4(1),
        outer_cipher_config: OuterCipherConfig::Twofish,
        compression_config: CompressionConfig::None,
        inner_cipher_config: InnerCipherConfig::Salsa20,
        kdf_config: KdfConfig::Aes { rounds: 42_000 },
        public_custom_data: None,
    }
}

fn build_kdbx_bytes_with_config(db: &StorageDatabase, config: DatabaseConfig) -> Vec<u8> {
    let creds = test_credentials(None);
    let keepass_db =
        storage_to_db_with_config(db, config).expect("failed to build keepass database");
    let mut bytes = Vec::new();
    keepass_db
        .save(&mut bytes, kdbx_git::kdbx::make_key(&creds).unwrap())
        .expect("failed to save KDBX bytes");
    bytes
}

fn keepassxc_db_info(path: &Path) -> String {
    let mut child = Command::new("keepassxc-cli")
        .args([
            "db-info",
            "-q",
            path.to_str().expect("path should be UTF-8"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn keepassxc-cli");

    child
        .stdin
        .as_mut()
        .expect("stdin should be available")
        .write_all(format!("{MASTER_PASSWORD}\n").as_bytes())
        .expect("failed to write keepassxc-cli password");

    let output = child
        .wait_with_output()
        .expect("failed to wait for keepassxc-cli");
    assert!(
        output.status.success(),
        "keepassxc-cli db-info failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("db-info output should be UTF-8")
}

fn info_line(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("missing '{prefix}' in keepassxc-cli output: {output}"))
        .to_string()
}

fn read_database_config(mut bytes: &[u8]) -> DatabaseConfig {
    let creds = test_credentials(None);
    Database::open(&mut bytes, kdbx_git::kdbx::make_key(&creds).unwrap())
        .expect("failed to reopen KDBX")
        .config
}

fn assert_roundtrip_crypto_config(
    source_path: &Path,
    roundtrip_path: &Path,
    source_bytes: &[u8],
    roundtrip_bytes: &[u8],
) {
    let source_info = keepassxc_db_info(source_path);
    let roundtrip_info = keepassxc_db_info(roundtrip_path);

    assert_eq!(
        info_line(&roundtrip_info, "Cipher:"),
        info_line(&source_info, "Cipher:")
    );
    assert_eq!(
        info_line(&roundtrip_info, "KDF:"),
        info_line(&source_info, "KDF:")
    );

    let source_config = read_database_config(source_bytes);
    let roundtrip_config = read_database_config(roundtrip_bytes);
    assert_eq!(
        roundtrip_config.outer_cipher_config,
        source_config.outer_cipher_config
    );
    assert_eq!(
        roundtrip_config.compression_config,
        source_config.compression_config
    );
    assert_eq!(
        roundtrip_config.inner_cipher_config,
        source_config.inner_cipher_config
    );
    assert_eq!(roundtrip_config.kdf_config, source_config.kdf_config);
}

async fn fetch_database(
    base_url: &str,
    client_id: &str,
    username: &str,
    password: &str,
) -> Vec<u8> {
    let client = Client::new();
    let response = client
        .get(format!("{base_url}/dav/{client_id}/database.kdbx"))
        .basic_auth(username, Some(password))
        .send()
        .await
        .expect("GET should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    response
        .bytes()
        .await
        .expect("body should be readable")
        .to_vec()
}

#[tokio::test]
async fn init_roundtrip_preserves_crypto_config() {
    let tempdir = TempDir::new().unwrap();
    let source_db = sample_db("Imported DB", "Imported Entry");
    let source_path = tempdir.path().join("source.kdbx");
    let roundtrip_path = tempdir.path().join("roundtrip.kdbx");
    let config = test_config(tempdir.path(), Some(source_path.clone()));
    let config_path = tempdir.path().join("config.toml");

    let source_bytes = build_kdbx_bytes_with_config(&source_db, non_default_kdbx_config());
    std::fs::write(&source_path, &source_bytes).unwrap();
    write_config(&config_path, &config);

    init_from_config_path(&config_path).await.unwrap();

    let server = TestServer::start(config, tempdir).await.unwrap();
    let roundtrip_bytes = fetch_database(&server.base_url, "bob", "bob-user", "bob-pass").await;
    std::fs::write(&roundtrip_path, &roundtrip_bytes).unwrap();

    assert_roundtrip_crypto_config(
        &source_path,
        &roundtrip_path,
        &source_bytes,
        &roundtrip_bytes,
    );
}

#[tokio::test]
async fn webdav_roundtrip_preserves_uploaded_crypto_config() {
    let tempdir = TempDir::new().unwrap();
    let source_path = tempdir.path().join("uploaded.kdbx");
    let roundtrip_path = tempdir.path().join("roundtrip.kdbx");
    let config = test_config(tempdir.path(), None);
    let server = TestServer::start(config, tempdir).await.unwrap();

    let source_db = sample_db("Uploaded DB", "Uploaded Entry");
    let source_bytes = build_kdbx_bytes_with_config(&source_db, non_default_kdbx_config());
    std::fs::write(&source_path, &source_bytes).unwrap();

    let client = Client::new();
    let put = client
        .put(format!("{}/dav/alice/database.kdbx", server.base_url))
        .basic_auth("alice-user", Some("alice-pass"))
        .body(source_bytes.clone())
        .send()
        .await
        .expect("PUT should succeed");
    assert!(
        put.status().is_success(),
        "unexpected PUT status: {}",
        put.status()
    );

    let roundtrip_bytes = fetch_database(&server.base_url, "bob", "bob-user", "bob-pass").await;
    std::fs::write(&roundtrip_path, &roundtrip_bytes).unwrap();

    assert_roundtrip_crypto_config(
        &source_path,
        &roundtrip_path,
        &source_bytes,
        &roundtrip_bytes,
    );
}
