//! Text (de)serialization of [`StorageDatabase`] to/from JSON, YAML, or TOML.

use super::types::{StorageAutoType, StorageDatabase, StorageEntry, StorageGroup};
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
    let canonical = canonicalize_for_serialization(db)?;
    match format {
        StorageFormat::Json => serde_json::to_string_pretty(&canonical)
            .wrap_err("failed to serialize database as JSON"),
        StorageFormat::Yaml => {
            serde_yaml::to_string(&canonical).wrap_err("failed to serialize database as YAML")
        }
        StorageFormat::Toml => {
            toml::to_string_pretty(&canonical).wrap_err("failed to serialize database as TOML")
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

fn canonicalize_for_serialization(db: &StorageDatabase) -> Result<StorageDatabase> {
    let mut canonical = db.clone();
    canonicalize_group(&mut canonical.root)?;
    Ok(canonical)
}

fn canonicalize_group(group: &mut StorageGroup) -> Result<()> {
    group.tags.sort_unstable();
    for subgroup in &mut group.groups {
        canonicalize_group(subgroup)?;
    }
    for entry in &mut group.entries {
        canonicalize_entry(entry)?;
    }
    Ok(())
}

fn canonicalize_entry(entry: &mut StorageEntry) -> Result<()> {
    entry.tags.sort_unstable();
    if let Some(autotype) = entry.autotype.as_mut() {
        canonicalize_autotype(autotype);
    }

    let mut keyed_history = entry
        .history
        .drain(..)
        .map(|mut historical_entry| {
            canonicalize_entry(&mut historical_entry)?;
            let key = serde_json::to_string(&historical_entry)
                .wrap_err("failed to canonicalize history entry ordering")?;
            Ok((key, historical_entry))
        })
        .collect::<Result<Vec<_>>>()?;
    keyed_history.sort_by(|(left_key, _), (right_key, _)| left_key.cmp(right_key));
    entry.history = keyed_history
        .into_iter()
        .map(|(_, historical_entry)| historical_entry)
        .collect();

    Ok(())
}

fn canonicalize_autotype(autotype: &mut StorageAutoType) {
    autotype.associations.sort_by(|left, right| {
        left.window
            .cmp(&right.window)
            .then(left.sequence.cmp(&right.sequence))
    });
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

    fn entry_with_history(
        history_order: &[(&str, &str)],
        group_tags: &[&str],
        entry_tags: &[&str],
        association_order: &[(&str, &str)],
    ) -> StorageDatabase {
        let history = history_order
            .iter()
            .map(|(uuid, title)| StorageEntry {
                uuid: (*uuid).into(),
                fields: BTreeMap::from([(
                    "Title".into(),
                    StorageValue {
                        value: (*title).into(),
                        protected: false,
                    },
                )]),
                autotype: None,
                tags: vec!["history".into()],
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
            })
            .collect();

        StorageDatabase {
            meta: StorageMeta {
                generator: Some("kdbx-git".into()),
                database_name: Some("Canonical Order Test".into()),
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
                    fields: BTreeMap::from([(
                        "Title".into(),
                        StorageValue {
                            value: "Current".into(),
                            protected: false,
                        },
                    )]),
                    autotype: Some(StorageAutoType {
                        enabled: true,
                        default_sequence: None,
                        data_transfer_obfuscation: None,
                        associations: association_order
                            .iter()
                            .map(|(window, sequence)| StorageAutoTypeAssociation {
                                window: (*window).into(),
                                sequence: (*sequence).into(),
                            })
                            .collect(),
                    }),
                    tags: entry_tags.iter().map(|tag| (*tag).into()).collect(),
                    times: StorageTimes {
                        creation: Some("2024-01-02T00:00:00".into()),
                        last_modification: Some("2024-01-02T00:00:00".into()),
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
                    history,
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
                tags: group_tags.iter().map(|tag| (*tag).into()).collect(),
                previous_parent_group: None,
            },
            deleted_objects: BTreeMap::new(),
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

    #[test]
    fn serialization_canonicalizes_unordered_collections() {
        let first = entry_with_history(
            &[
                ("00000000-0000-0000-0000-000000000030", "z-history"),
                ("00000000-0000-0000-0000-000000000010", "a-history"),
                ("00000000-0000-0000-0000-000000000020", "m-history"),
            ],
            &["ops", "admin", "finance"],
            &["prod", "shared", "billing"],
            &[("Z Window", "{TAB}"), ("A Window", "{ENTER}")],
        );
        let second = entry_with_history(
            &[
                ("00000000-0000-0000-0000-000000000020", "m-history"),
                ("00000000-0000-0000-0000-000000000030", "z-history"),
                ("00000000-0000-0000-0000-000000000010", "a-history"),
            ],
            &["finance", "ops", "admin"],
            &["billing", "prod", "shared"],
            &[("A Window", "{ENTER}"), ("Z Window", "{TAB}")],
        );

        for format in StorageFormat::ALL {
            let first_serialized = serialize(&first, format).unwrap();
            let second_serialized = serialize(&second, format).unwrap();
            assert_eq!(
                first_serialized, second_serialized,
                "expected canonicalized {format:?} serialization for unordered collections"
            );
        }
    }
}
