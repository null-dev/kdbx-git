//! Serializable mirror of the keepass database model.
//!
//! These structs are stored as indented JSON (or YAML/TOML) blobs inside the
//! git object store.  Every field that has a keepass counterpart is
//! represented here; binary data is base64-encoded so the text is diff-able.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root document stored per git commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDatabase {
    pub meta: StorageMeta,
    pub root: StorageGroup,
    /// UUID (string) → deletion timestamp (ISO 8601) or `null`.
    pub deleted_objects: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageMeta {
    pub generator: Option<String>,
    pub database_name: Option<String>,
    pub database_name_changed: Option<String>,
    pub database_description: Option<String>,
    pub database_description_changed: Option<String>,
    pub default_username: Option<String>,
    pub default_username_changed: Option<String>,
    pub maintenance_history_days: Option<usize>,
    pub color: Option<StorageColor>,
    pub master_key_changed: Option<String>,
    pub master_key_change_rec: Option<i64>,
    pub master_key_change_force: Option<i64>,
    pub memory_protection: Option<StorageMemoryProtection>,
    pub recyclebin_enabled: Option<bool>,
    pub recyclebin_uuid: Option<String>,
    pub recyclebin_changed: Option<String>,
    pub entry_templates_group: Option<String>,
    pub entry_templates_group_changed: Option<String>,
    pub last_selected_group: Option<String>,
    pub last_top_visible_group: Option<String>,
    pub history_max_items: Option<usize>,
    pub history_max_size: Option<usize>,
    pub settings_changed: Option<String>,
    pub custom_data: HashMap<String, StorageCustomDataItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageMemoryProtection {
    pub protect_title: bool,
    pub protect_username: bool,
    pub protect_password: bool,
    pub protect_url: bool,
    pub protect_notes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageGroup {
    pub uuid: String,
    pub name: String,
    pub notes: Option<String>,
    pub icon_id: Option<usize>,
    pub custom_icon: Option<StorageCustomIcon>,
    pub groups: Vec<StorageGroup>,
    pub entries: Vec<StorageEntry>,
    pub times: StorageTimes,
    pub custom_data: HashMap<String, StorageCustomDataItem>,
    pub is_expanded: bool,
    pub default_autotype_sequence: Option<String>,
    pub enable_autotype: Option<bool>,
    pub enable_searching: Option<bool>,
    pub last_top_visible_entry: Option<String>,
    pub tags: Vec<String>,
    pub previous_parent_group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    pub uuid: String,
    pub fields: HashMap<String, StorageValue>,
    pub autotype: Option<StorageAutoType>,
    pub tags: Vec<String>,
    pub times: StorageTimes,
    pub custom_data: HashMap<String, StorageCustomDataItem>,
    pub icon_id: Option<usize>,
    pub custom_icon: Option<StorageCustomIcon>,
    pub foreground_color: Option<StorageColor>,
    pub background_color: Option<StorageColor>,
    pub override_url: Option<String>,
    pub quality_check: Option<bool>,
    pub previous_parent_group: Option<String>,
    /// Attachment name → base64-encoded binary blob.
    pub attachments: HashMap<String, StorageAttachment>,
    /// Previous versions of this entry (no further nesting).
    pub history: Vec<StorageEntry>,
}

/// A KeePass entry field value; the `protected` flag drives in-memory
/// protection when the database is reconstructed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageValue {
    pub value: String,
    pub protected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageTimes {
    pub creation: Option<String>,
    pub last_modification: Option<String>,
    pub last_access: Option<String>,
    pub expiry: Option<String>,
    pub location_changed: Option<String>,
    pub expires: Option<bool>,
    pub usage_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageAutoType {
    pub enabled: bool,
    pub default_sequence: Option<String>,
    pub data_transfer_obfuscation: Option<bool>,
    pub associations: Vec<StorageAutoTypeAssociation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageAutoTypeAssociation {
    pub window: String,
    pub sequence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Custom icon; PNG bytes are base64-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCustomIcon {
    pub uuid: String,
    /// Base64-encoded PNG image data.
    pub data: String,
    pub name: Option<String>,
    pub last_modification_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCustomDataItem {
    pub value: Option<StorageCustomDataValue>,
    pub last_modification_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StorageCustomDataValue {
    String(String),
    /// Base64-encoded binary data.
    Binary(String),
}

/// Binary attachment; bytes are base64-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageAttachment {
    /// Base64-encoded attachment data.
    pub data: String,
    pub protected: bool,
}
