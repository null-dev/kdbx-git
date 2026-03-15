//! Conversions between the keepass database model and [`StorageDatabase`].

use super::types::*;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::NaiveDateTime;
use eyre::{Context, Result};
use keepass::{
    db::{
        Attachment, AutoType, AutoTypeAssociation, Color, CustomDataItem, CustomDataValue,
        CustomIcon, Entry, Group, History, MemoryProtection, Meta, Times, Value,
    },
    Database,
};
use std::collections::HashMap;
use uuid::Uuid;

// ── Timestamp helpers ────────────────────────────────────────────────────────

const DT_FMT: &str = "%Y-%m-%dT%H:%M:%S";

fn dt_to_str(dt: NaiveDateTime) -> String {
    dt.format(DT_FMT).to_string()
}

fn str_to_dt(s: &str) -> Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, DT_FMT)
        .wrap_err_with(|| format!("invalid datetime: {s}"))
}

fn opt_dt(dt: Option<NaiveDateTime>) -> Option<String> {
    dt.map(dt_to_str)
}

fn opt_str_dt(s: Option<&str>) -> Result<Option<NaiveDateTime>> {
    s.map(str_to_dt).transpose()
}

// ── UUID helpers ─────────────────────────────────────────────────────────────

fn u2s(u: Uuid) -> String {
    u.to_string()
}

fn s2u(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).wrap_err_with(|| format!("invalid UUID: {s}"))
}

// ── Base64 helpers ────────────────────────────────────────────────────────────

fn b64(b: &[u8]) -> String {
    BASE64.encode(b)
}

fn from_b64(s: &str) -> Result<Vec<u8>> {
    BASE64.decode(s).wrap_err("invalid base64")
}

// ── Database ─────────────────────────────────────────────────────────────────

/// Convert a keepass [`Database`] into its storage representation.
pub fn db_to_storage(db: &Database) -> Result<StorageDatabase> {
    let deleted = db
        .deleted_objects
        .iter()
        .map(|(uuid, dt)| (u2s(*uuid), dt.map(dt_to_str)))
        .collect();

    Ok(StorageDatabase {
        meta: meta_to_storage(&db.meta)?,
        root: group_to_storage(&db.root)?,
        deleted_objects: deleted,
    })
}

/// Reconstruct a keepass [`Database`] from storage.
///
/// The caller supplies the [`keepass::config::DatabaseConfig`] so that
/// cipher / KDF settings can be chosen independently of this layer.
pub fn storage_to_db(
    s: &StorageDatabase,
    config: keepass::config::DatabaseConfig,
) -> Result<Database> {
    let mut deleted = HashMap::new();
    for (uuid_str, dt_str) in &s.deleted_objects {
        deleted.insert(s2u(uuid_str)?, opt_str_dt(dt_str.as_deref())?);
    }

    let mut db = Database::new(config);
    db.meta = storage_to_meta(&s.meta)?;
    db.root = storage_to_group(&s.root)?;
    db.deleted_objects = deleted;
    Ok(db)
}

// ── Meta ─────────────────────────────────────────────────────────────────────

fn meta_to_storage(m: &Meta) -> Result<StorageMeta> {
    Ok(StorageMeta {
        generator: m.generator.clone(),
        database_name: m.database_name.clone(),
        database_name_changed: opt_dt(m.database_name_changed),
        database_description: m.database_description.clone(),
        database_description_changed: opt_dt(m.database_description_changed),
        default_username: m.default_username.clone(),
        default_username_changed: opt_dt(m.default_username_changed),
        maintenance_history_days: m.maintenance_history_days,
        color: m.color.as_ref().map(color_to_storage),
        master_key_changed: opt_dt(m.master_key_changed),
        master_key_change_rec: m.master_key_change_rec.map(|x| x as i64),
        master_key_change_force: m.master_key_change_force.map(|x| x as i64),
        memory_protection: m.memory_protection.as_ref().map(memprotect_to_storage),
        recyclebin_enabled: m.recyclebin_enabled,
        recyclebin_uuid: m.recyclebin_uuid.map(u2s),
        recyclebin_changed: opt_dt(m.recyclebin_changed),
        entry_templates_group: m.entry_templates_group.map(u2s),
        entry_templates_group_changed: opt_dt(m.entry_templates_group_changed),
        last_selected_group: m.last_selected_group.map(u2s),
        last_top_visible_group: m.last_top_visible_group.map(u2s),
        history_max_items: m.history_max_items,
        history_max_size: m.history_max_size,
        settings_changed: opt_dt(m.settings_changed),
        custom_data: custom_data_map_to_storage(&m.custom_data)?,
    })
}

fn storage_to_meta(s: &StorageMeta) -> Result<Meta> {
    Ok(Meta {
        generator: s.generator.clone(),
        database_name: s.database_name.clone(),
        database_name_changed: opt_str_dt(s.database_name_changed.as_deref())?,
        database_description: s.database_description.clone(),
        database_description_changed: opt_str_dt(s.database_description_changed.as_deref())?,
        default_username: s.default_username.clone(),
        default_username_changed: opt_str_dt(s.default_username_changed.as_deref())?,
        maintenance_history_days: s.maintenance_history_days,
        color: s.color.as_ref().map(storage_to_color),
        master_key_changed: opt_str_dt(s.master_key_changed.as_deref())?,
        master_key_change_rec: s.master_key_change_rec.map(|x| x as isize),
        master_key_change_force: s.master_key_change_force.map(|x| x as isize),
        memory_protection: s.memory_protection.as_ref().map(storage_to_memprotect),
        recyclebin_enabled: s.recyclebin_enabled,
        recyclebin_uuid: s.recyclebin_uuid.as_deref().map(s2u).transpose()?,
        recyclebin_changed: opt_str_dt(s.recyclebin_changed.as_deref())?,
        entry_templates_group: s.entry_templates_group.as_deref().map(s2u).transpose()?,
        entry_templates_group_changed: opt_str_dt(s.entry_templates_group_changed.as_deref())?,
        last_selected_group: s.last_selected_group.as_deref().map(s2u).transpose()?,
        last_top_visible_group: s.last_top_visible_group.as_deref().map(s2u).transpose()?,
        history_max_items: s.history_max_items,
        history_max_size: s.history_max_size,
        settings_changed: opt_str_dt(s.settings_changed.as_deref())?,
        custom_data: storage_to_custom_data_map(&s.custom_data)?,
    })
}

// ── MemoryProtection ─────────────────────────────────────────────────────────

fn memprotect_to_storage(m: &MemoryProtection) -> StorageMemoryProtection {
    StorageMemoryProtection {
        protect_title: m.protect_title,
        protect_username: m.protect_username,
        protect_password: m.protect_password,
        protect_url: m.protect_url,
        protect_notes: m.protect_notes,
    }
}

fn storage_to_memprotect(s: &StorageMemoryProtection) -> MemoryProtection {
    MemoryProtection {
        protect_title: s.protect_title,
        protect_username: s.protect_username,
        protect_password: s.protect_password,
        protect_url: s.protect_url,
        protect_notes: s.protect_notes,
    }
}

// ── Group ────────────────────────────────────────────────────────────────────

fn group_to_storage(g: &Group) -> Result<StorageGroup> {
    Ok(StorageGroup {
        uuid: u2s(g.uuid),
        name: g.name.clone(),
        notes: g.notes.clone(),
        icon_id: g.icon_id,
        custom_icon: g.custom_icon.as_ref().map(custom_icon_to_storage),
        groups: g
            .groups
            .iter()
            .map(group_to_storage)
            .collect::<Result<_>>()?,
        entries: g
            .entries
            .iter()
            .map(entry_to_storage)
            .collect::<Result<_>>()?,
        times: times_to_storage(&g.times),
        custom_data: custom_data_map_to_storage(&g.custom_data)?,
        is_expanded: g.is_expanded,
        default_autotype_sequence: g.default_autotype_sequence.clone(),
        enable_autotype: g.enable_autotype,
        enable_searching: g.enable_searching,
        last_top_visible_entry: g.last_top_visible_entry.map(u2s),
        tags: g.tags.clone(),
        previous_parent_group: g.previous_parent_group.map(u2s),
    })
}

fn storage_to_group(s: &StorageGroup) -> Result<Group> {
    Ok(Group {
        uuid: s2u(&s.uuid)?,
        name: s.name.clone(),
        notes: s.notes.clone(),
        icon_id: s.icon_id,
        custom_icon: s
            .custom_icon
            .as_ref()
            .map(storage_to_custom_icon)
            .transpose()?,
        groups: s
            .groups
            .iter()
            .map(storage_to_group)
            .collect::<Result<_>>()?,
        entries: s
            .entries
            .iter()
            .map(storage_to_entry)
            .collect::<Result<_>>()?,
        times: storage_to_times(&s.times)?,
        custom_data: storage_to_custom_data_map(&s.custom_data)?,
        is_expanded: s.is_expanded,
        default_autotype_sequence: s.default_autotype_sequence.clone(),
        enable_autotype: s.enable_autotype,
        enable_searching: s.enable_searching,
        last_top_visible_entry: s
            .last_top_visible_entry
            .as_deref()
            .map(s2u)
            .transpose()?,
        tags: s.tags.clone(),
        previous_parent_group: s.previous_parent_group.as_deref().map(s2u).transpose()?,
    })
}

// ── Entry ────────────────────────────────────────────────────────────────────

fn entry_to_storage(e: &Entry) -> Result<StorageEntry> {
    let history = match &e.history {
        Some(h) => h
            .get_entries()
            .iter()
            .map(entry_to_storage)
            .collect::<Result<_>>()?,
        None => vec![],
    };

    let fields = e
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), value_str_to_storage(v)))
        .collect();

    let attachments = e
        .attachments
        .iter()
        .map(|(name, att)| (name.clone(), attachment_to_storage(att)))
        .collect();

    Ok(StorageEntry {
        uuid: u2s(e.uuid),
        fields,
        autotype: e.autotype.as_ref().map(autotype_to_storage),
        tags: e.tags.clone(),
        times: times_to_storage(&e.times),
        custom_data: custom_data_map_to_storage(&e.custom_data)?,
        icon_id: e.icon_id,
        custom_icon: e.custom_icon.as_ref().map(custom_icon_to_storage),
        foreground_color: e.foreground_color.as_ref().map(color_to_storage),
        background_color: e.background_color.as_ref().map(color_to_storage),
        override_url: e.override_url.clone(),
        quality_check: e.quality_check,
        previous_parent_group: e.previous_parent_group.map(u2s),
        attachments,
        history,
    })
}

fn storage_to_entry(s: &StorageEntry) -> Result<Entry> {
    let history = if s.history.is_empty() {
        None
    } else {
        let mut h = History::default();
        for se in &s.history {
            h.add_entry(storage_to_entry(se)?);
        }
        Some(h)
    };

    let fields = s
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), storage_to_value_str(v)))
        .collect();

    let attachments = s
        .attachments
        .iter()
        .map(|(name, att)| storage_to_attachment(att).map(|a| (name.clone(), a)))
        .collect::<Result<_>>()?;

    Ok(Entry {
        uuid: s2u(&s.uuid)?,
        fields,
        autotype: s.autotype.as_ref().map(storage_to_autotype),
        tags: s.tags.clone(),
        times: storage_to_times(&s.times)?,
        custom_data: storage_to_custom_data_map(&s.custom_data)?,
        icon_id: s.icon_id,
        custom_icon: s
            .custom_icon
            .as_ref()
            .map(storage_to_custom_icon)
            .transpose()?,
        foreground_color: s.foreground_color.as_ref().map(storage_to_color),
        background_color: s.background_color.as_ref().map(storage_to_color),
        override_url: s.override_url.clone(),
        quality_check: s.quality_check,
        previous_parent_group: s.previous_parent_group.as_deref().map(s2u).transpose()?,
        attachments,
        history,
    })
}

// ── Value<String> ─────────────────────────────────────────────────────────────

fn value_str_to_storage(v: &Value<String>) -> StorageValue {
    StorageValue {
        value: v.get().clone(),
        protected: v.is_protected(),
    }
}

fn storage_to_value_str(s: &StorageValue) -> Value<String> {
    if s.protected {
        Value::protected(s.value.clone())
    } else {
        Value::unprotected(s.value.clone())
    }
}

// ── Attachment ────────────────────────────────────────────────────────────────

fn attachment_to_storage(a: &Attachment) -> StorageAttachment {
    StorageAttachment {
        data: b64(a.data.get()),
        protected: a.data.is_protected(),
    }
}

fn storage_to_attachment(s: &StorageAttachment) -> Result<Attachment> {
    let bytes = from_b64(&s.data)?;
    Ok(Attachment {
        data: if s.protected {
            Value::protected(bytes)
        } else {
            Value::unprotected(bytes)
        },
    })
}

// ── Times ─────────────────────────────────────────────────────────────────────

fn times_to_storage(t: &Times) -> StorageTimes {
    StorageTimes {
        creation: opt_dt(t.creation),
        last_modification: opt_dt(t.last_modification),
        last_access: opt_dt(t.last_access),
        expiry: opt_dt(t.expiry),
        location_changed: opt_dt(t.location_changed),
        expires: t.expires,
        usage_count: t.usage_count,
    }
}

fn storage_to_times(s: &StorageTimes) -> Result<Times> {
    // Times is #[non_exhaustive] so it must be built via field assignment.
    let mut t = Times::default();
    t.creation = opt_str_dt(s.creation.as_deref())?;
    t.last_modification = opt_str_dt(s.last_modification.as_deref())?;
    t.last_access = opt_str_dt(s.last_access.as_deref())?;
    t.expiry = opt_str_dt(s.expiry.as_deref())?;
    t.location_changed = opt_str_dt(s.location_changed.as_deref())?;
    t.expires = s.expires;
    t.usage_count = s.usage_count;
    Ok(t)
}

// ── AutoType ──────────────────────────────────────────────────────────────────

fn autotype_to_storage(a: &AutoType) -> StorageAutoType {
    StorageAutoType {
        enabled: a.enabled,
        default_sequence: a.default_sequence.clone(),
        data_transfer_obfuscation: a.data_transfer_obfuscation,
        associations: a
            .associations
            .iter()
            .map(|assoc| StorageAutoTypeAssociation {
                window: assoc.window.clone(),
                sequence: assoc.sequence.clone(),
            })
            .collect(),
    }
}

fn storage_to_autotype(s: &StorageAutoType) -> AutoType {
    AutoType {
        enabled: s.enabled,
        default_sequence: s.default_sequence.clone(),
        data_transfer_obfuscation: s.data_transfer_obfuscation,
        associations: s
            .associations
            .iter()
            .map(|a| AutoTypeAssociation {
                window: a.window.clone(),
                sequence: a.sequence.clone(),
            })
            .collect(),
    }
}

// ── Color ─────────────────────────────────────────────────────────────────────

fn color_to_storage(c: &Color) -> StorageColor {
    StorageColor {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}

fn storage_to_color(s: &StorageColor) -> Color {
    Color {
        r: s.r,
        g: s.g,
        b: s.b,
    }
}

// ── CustomIcon ────────────────────────────────────────────────────────────────

fn custom_icon_to_storage(i: &CustomIcon) -> StorageCustomIcon {
    StorageCustomIcon {
        uuid: u2s(i.uuid),
        data: b64(&i.data),
        name: i.name.clone(),
        last_modification_time: opt_dt(i.last_modification_time),
    }
}

fn storage_to_custom_icon(s: &StorageCustomIcon) -> Result<CustomIcon> {
    Ok(CustomIcon {
        uuid: s2u(&s.uuid)?,
        data: from_b64(&s.data)?,
        name: s.name.clone(),
        last_modification_time: opt_str_dt(s.last_modification_time.as_deref())?,
    })
}

// ── CustomData ────────────────────────────────────────────────────────────────

fn custom_data_map_to_storage(
    m: &HashMap<String, CustomDataItem>,
) -> Result<HashMap<String, StorageCustomDataItem>> {
    m.iter()
        .map(|(k, v)| Ok((k.clone(), custom_data_item_to_storage(v))))
        .collect()
}

fn storage_to_custom_data_map(
    m: &HashMap<String, StorageCustomDataItem>,
) -> Result<HashMap<String, CustomDataItem>> {
    m.iter()
        .map(|(k, v)| Ok((k.clone(), storage_to_custom_data_item(v)?)))
        .collect()
}

fn custom_data_item_to_storage(item: &CustomDataItem) -> StorageCustomDataItem {
    StorageCustomDataItem {
        value: item.value.as_ref().map(custom_data_value_to_storage),
        last_modification_time: opt_dt(item.last_modification_time),
    }
}

fn storage_to_custom_data_item(s: &StorageCustomDataItem) -> Result<CustomDataItem> {
    Ok(CustomDataItem {
        value: s
            .value
            .as_ref()
            .map(storage_to_custom_data_value)
            .transpose()?,
        last_modification_time: opt_str_dt(s.last_modification_time.as_deref())?,
    })
}

fn custom_data_value_to_storage(v: &CustomDataValue) -> StorageCustomDataValue {
    match v {
        CustomDataValue::String(s) => StorageCustomDataValue::String(s.clone()),
        CustomDataValue::Binary(b) => StorageCustomDataValue::Binary(b64(b)),
    }
}

fn storage_to_custom_data_value(s: &StorageCustomDataValue) -> Result<CustomDataValue> {
    match s {
        StorageCustomDataValue::String(s) => Ok(CustomDataValue::String(s.clone())),
        StorageCustomDataValue::Binary(b) => Ok(CustomDataValue::Binary(from_b64(b)?)),
    }
}
