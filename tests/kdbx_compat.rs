mod common;

use common::{build_kdbx_bytes, sample_db, test_credentials};
use kdbx_git::storage::types::{StorageCustomDataItem, StorageCustomDataValue};
use keepass::Database;

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
