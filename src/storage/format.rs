//! Text (de)serialization of [`StorageDatabase`] to/from JSON, YAML, or TOML.

use super::types::StorageDatabase;
use eyre::{Context, Result};

/// The text format used to store database content in git commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageFormat {
    /// Pretty-printed JSON (default; most tooling-friendly).
    #[default]
    Json,
    /// YAML.
    Yaml,
    /// TOML.
    Toml,
}

impl StorageFormat {
    pub const ALL: [Self; 3] = [Self::Json, Self::Yaml, Self::Toml];

    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
        }
    }

    /// File name used for the database blob in every git tree.
    pub fn file_name(self) -> String {
        format!("db.{}", self.file_extension())
    }
}

/// Serialize `db` to a UTF-8 string in `format`.
pub fn serialize(db: &StorageDatabase, format: StorageFormat) -> Result<String> {
    match format {
        StorageFormat::Json => {
            serde_json::to_string_pretty(db).wrap_err("failed to serialize database as JSON")
        }
        StorageFormat::Yaml => {
            serde_yaml::to_string(db).wrap_err("failed to serialize database as YAML")
        }
        StorageFormat::Toml => {
            toml::to_string_pretty(db).wrap_err("failed to serialize database as TOML")
        }
    }
}

/// Deserialize a [`StorageDatabase`] from a UTF-8 string in `format`.
pub fn deserialize(s: &str, format: StorageFormat) -> Result<StorageDatabase> {
    match format {
        StorageFormat::Json => {
            serde_json::from_str(s).wrap_err("failed to deserialize database from JSON")
        }
        StorageFormat::Yaml => {
            serde_yaml::from_str(s).wrap_err("failed to deserialize database from YAML")
        }
        StorageFormat::Toml => {
            toml::from_str(s).wrap_err("failed to deserialize database from TOML")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::types::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn minimal_db() -> StorageDatabase {
        StorageDatabase {
            meta: StorageMeta {
                generator: Some("kdbx-git".into()),
                database_name: Some("Test DB".into()),
                database_name_changed: None,
                database_description: None,
                database_description_changed: None,
                default_username: None,
                default_username_changed: None,
                maintenance_history_days: None,
                color: None,
                master_key_changed: None,
                master_key_change_rec: None,
                master_key_change_force: None,
                memory_protection: Some(StorageMemoryProtection {
                    protect_title: false,
                    protect_username: false,
                    protect_password: true,
                    protect_url: false,
                    protect_notes: false,
                }),
                recyclebin_enabled: Some(true),
                recyclebin_uuid: None,
                recyclebin_changed: None,
                entry_templates_group: None,
                entry_templates_group_changed: None,
                last_selected_group: None,
                last_top_visible_group: None,
                history_max_items: Some(10),
                history_max_size: Some(6_291_456),
                settings_changed: None,
                custom_data: BTreeMap::new(),
            },
            root: StorageGroup {
                uuid: "00000000-0000-0000-0000-000000000001".into(),
                name: "Root".into(),
                notes: None,
                icon_id: None,
                custom_icon: None,
                groups: vec![],
                entries: vec![StorageEntry {
                    uuid: "00000000-0000-0000-0000-000000000002".into(),
                    fields: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "Title".into(),
                            StorageValue {
                                value: "Example".into(),
                                protected: false,
                            },
                        );
                        m.insert(
                            "Password".into(),
                            StorageValue {
                                value: "s3cr3t".into(),
                                protected: true,
                            },
                        );
                        m
                    },
                    autotype: None,
                    tags: vec![],
                    times: StorageTimes {
                        creation: Some("2024-01-01T00:00:00".into()),
                        last_modification: Some("2024-01-01T00:00:00".into()),
                        last_access: None,
                        expiry: None,
                        location_changed: None,
                        expires: Some(false),
                        usage_count: Some(0),
                    },
                    custom_data: BTreeMap::new(),
                    icon_id: None,
                    custom_icon: None,
                    foreground_color: None,
                    background_color: None,
                    override_url: None,
                    quality_check: None,
                    previous_parent_group: None,
                    attachments: BTreeMap::new(),
                    history: vec![],
                }],
                times: StorageTimes {
                    creation: None,
                    last_modification: None,
                    last_access: None,
                    expiry: None,
                    location_changed: None,
                    expires: None,
                    usage_count: None,
                },
                custom_data: BTreeMap::new(),
                is_expanded: true,
                default_autotype_sequence: None,
                enable_autotype: None,
                enable_searching: None,
                last_top_visible_entry: None,
                tags: vec![],
                previous_parent_group: None,
            },
            deleted_objects: BTreeMap::new(),
        }
    }

    fn map_order_db() -> StorageDatabase {
        let mut meta_custom_data = BTreeMap::new();
        for key in ["z-last", "m-middle", "a-first", "k-other"] {
            meta_custom_data.insert(
                key.into(),
                StorageCustomDataItem {
                    value: Some(StorageCustomDataValue::String(format!("value-{key}"))),
                    last_modification_time: Some("2024-01-01T00:00:00".into()),
                },
            );
        }

        let mut group_custom_data = BTreeMap::new();
        for key in ["tag-z", "tag-c", "tag-a", "tag-m"] {
            group_custom_data.insert(
                key.into(),
                StorageCustomDataItem {
                    value: Some(StorageCustomDataValue::String(format!("group-{key}"))),
                    last_modification_time: None,
                },
            );
        }

        let mut entry_fields = BTreeMap::new();
        for key in ["UserName", "Password", "URL", "Notes", "Title"] {
            entry_fields.insert(
                key.into(),
                StorageValue {
                    value: format!("value-{key}"),
                    protected: key == "Password",
                },
            );
        }

        let mut entry_custom_data = BTreeMap::new();
        for key in ["delta", "beta", "alpha", "gamma"] {
            entry_custom_data.insert(
                key.into(),
                StorageCustomDataItem {
                    value: Some(StorageCustomDataValue::String(format!("entry-{key}"))),
                    last_modification_time: None,
                },
            );
        }

        let mut attachments = BTreeMap::new();
        for key in ["backup.bin", "a.txt", "notes.md", "z.log"] {
            attachments.insert(
                key.into(),
                StorageAttachment {
                    data: format!("encoded-{key}"),
                    protected: false,
                },
            );
        }

        let mut deleted_objects = BTreeMap::new();
        for key in [
            "00000000-0000-0000-0000-000000000003",
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000002",
        ] {
            deleted_objects.insert(key.into(), Some("2024-01-01T00:00:00".into()));
        }

        StorageDatabase {
            meta: StorageMeta {
                generator: Some("kdbx-git".into()),
                database_name: Some("Order Test".into()),
                database_name_changed: None,
                database_description: None,
                database_description_changed: None,
                default_username: None,
                default_username_changed: None,
                maintenance_history_days: None,
                color: None,
                master_key_changed: None,
                master_key_change_rec: None,
                master_key_change_force: None,
                memory_protection: None,
                recyclebin_enabled: Some(true),
                recyclebin_uuid: None,
                recyclebin_changed: None,
                entry_templates_group: None,
                entry_templates_group_changed: None,
                last_selected_group: None,
                last_top_visible_group: None,
                history_max_items: None,
                history_max_size: None,
                settings_changed: None,
                custom_data: meta_custom_data,
            },
            root: StorageGroup {
                uuid: "00000000-0000-0000-0000-000000000001".into(),
                name: "Root".into(),
                notes: None,
                icon_id: None,
                custom_icon: None,
                groups: vec![],
                entries: vec![StorageEntry {
                    uuid: "00000000-0000-0000-0000-000000000002".into(),
                    fields: entry_fields,
                    autotype: None,
                    tags: vec![],
                    times: StorageTimes {
                        creation: Some("2024-01-01T00:00:00".into()),
                        last_modification: Some("2024-01-01T00:00:00".into()),
                        last_access: None,
                        expiry: None,
                        location_changed: None,
                        expires: Some(false),
                        usage_count: Some(0),
                    },
                    custom_data: entry_custom_data,
                    icon_id: None,
                    custom_icon: None,
                    foreground_color: None,
                    background_color: None,
                    override_url: None,
                    quality_check: None,
                    previous_parent_group: None,
                    attachments,
                    history: vec![],
                }],
                times: StorageTimes {
                    creation: None,
                    last_modification: None,
                    last_access: None,
                    expiry: None,
                    location_changed: None,
                    expires: None,
                    usage_count: None,
                },
                custom_data: group_custom_data,
                is_expanded: true,
                default_autotype_sequence: None,
                enable_autotype: None,
                enable_searching: None,
                last_top_visible_entry: None,
                tags: vec![],
                previous_parent_group: None,
            },
            deleted_objects,
        }
    }

    #[test]
    fn roundtrip_all_supported_formats() {
        let db = minimal_db();
        for format in StorageFormat::ALL {
            let s = serialize(&db, format).unwrap();
            let db2 = deserialize(&s, format).unwrap();
            assert_eq!(db2.root.entries[0].fields["Password"].value, "s3cr3t");
            assert!(db2.root.entries[0].fields["Password"].protected);
        }
    }

    #[test]
    fn json_remains_the_default_storage_format() {
        assert_eq!(StorageFormat::default(), StorageFormat::Json);
        assert_eq!(StorageFormat::default().file_name(), "db.json");
    }

    #[test]
    fn serialization_is_deterministic_for_all_formats() {
        for format in StorageFormat::ALL {
            let variants = (0..24)
                .map(|_| serialize(&map_order_db(), format).unwrap())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                variants.len(),
                1,
                "expected deterministic {format:?} serialization, got {} variants",
                variants.len()
            );
        }
    }
}
