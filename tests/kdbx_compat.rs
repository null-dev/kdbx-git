mod common;

use common::{build_kdbx_bytes, sample_db, test_credentials};
use keepass::Database;

#[test]
fn emitted_kdbx_omits_unset_optional_bool_fields() {
    let db = sample_db("Compat DB", "Compat Entry");
    let creds = test_credentials(None);
    let bytes = build_kdbx_bytes(&db, &creds);

    let xml = Database::get_xml(&mut bytes.as_slice(), kdbx_git::kdbx::make_key(&creds).unwrap())
        .expect("failed to decrypt emitted KDBX");
    let xml = String::from_utf8(xml).expect("xml should be valid UTF-8");

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
    let creds = test_credentials(None);
    let bytes = build_kdbx_bytes(&db, &creds);

    let xml = Database::get_xml(&mut bytes.as_slice(), kdbx_git::kdbx::make_key(&creds).unwrap())
        .expect("failed to decrypt emitted KDBX");
    let xml = String::from_utf8(xml).expect("xml should be valid UTF-8");

    assert!(xml.contains("<EnableAutoType>False</EnableAutoType>"), "{xml}");
    assert!(xml.contains("<EnableSearching>True</EnableSearching>"), "{xml}");
}
