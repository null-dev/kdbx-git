use std::collections::{BTreeMap, BTreeSet};

use kdbx_git_storage_types::{StorageDatabase, StorageEntry, StorageGroup};
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const USERS_GROUP_NAME: &str = "KeeGate Users";
pub const BASIC_AUTH_REALM: &str = "KeeGate API";
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryEntriesRequest {
    pub filter: QueryFilterRequest,
    #[serde(default)]
    pub options: QueryOptionsRequest,
}

impl QueryEntriesRequest {
    pub fn validate(self) -> Result<ValidatedQuery, QueryValidationError> {
        Ok(ValidatedQuery {
            filter: self.filter.compile()?,
            limit: self.options.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryOptionsRequest {
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QueryFilterRequest {
    TitleContains(TitleContainsFilter),
    TitleRegex(TitleRegexFilter),
    Tag(TagFilter),
    Uuid(UuidFilter),
    And(AndFilter),
    Or(OrFilter),
}

impl QueryFilterRequest {
    fn compile(self) -> Result<EntryFilter, QueryValidationError> {
        match self {
            Self::TitleContains(filter) => Ok(EntryFilter::TitleContains(
                filter.title_contains.to_lowercase(),
            )),
            Self::TitleRegex(filter) => Regex::new(&filter.title_regex)
                .map(EntryFilter::TitleRegex)
                .map_err(|err| QueryValidationError::InvalidRegex(err.to_string())),
            Self::Tag(filter) => Ok(EntryFilter::Tag(filter.tag)),
            Self::Uuid(filter) => Uuid::parse_str(&filter.uuid)
                .map(EntryFilter::Uuid)
                .map_err(|err| QueryValidationError::InvalidUuid(err.to_string())),
            Self::And(filter) => {
                if filter.and.is_empty() {
                    return Err(QueryValidationError::EmptyBooleanFilter("and"));
                }
                let mut filters = Vec::with_capacity(filter.and.len());
                for child in filter.and {
                    filters.push(child.compile()?);
                }
                Ok(EntryFilter::And(filters))
            }
            Self::Or(filter) => {
                if filter.or.is_empty() {
                    return Err(QueryValidationError::EmptyBooleanFilter("or"));
                }
                let mut filters = Vec::with_capacity(filter.or.len());
                for child in filter.or {
                    filters.push(child.compile()?);
                }
                Ok(EntryFilter::Or(filters))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleContainsFilter {
    pub title_contains: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleRegexFilter {
    pub title_regex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFilter {
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UuidFilter {
    pub uuid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndFilter {
    pub and: Vec<QueryFilterRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrFilter {
    pub or: Vec<QueryFilterRequest>,
}

#[derive(Debug, Clone)]
pub struct ValidatedQuery {
    filter: EntryFilter,
    limit: usize,
}

impl ValidatedQuery {
    pub fn limit(&self) -> usize {
        self.limit
    }
}

#[derive(Debug, Clone)]
enum EntryFilter {
    TitleContains(String),
    TitleRegex(Regex),
    Tag(String),
    Uuid(Uuid),
    And(Vec<EntryFilter>),
    Or(Vec<EntryFilter>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub username: String,
    pub tags: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    InvalidCredentials,
    AmbiguousUsername,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCredentials => write!(f, "invalid KeeGate API credentials"),
            Self::AmbiguousUsername => write!(f, "ambiguous KeeGate API username"),
        }
    }
}

impl std::error::Error for AuthError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryValidationError {
    EmptyBooleanFilter(&'static str),
    InvalidRegex(String),
    InvalidUuid(String),
}

impl QueryValidationError {
    pub fn message(&self) -> String {
        match self {
            Self::EmptyBooleanFilter(op) => {
                format!("filter.{op} must contain at least one child filter")
            }
            Self::InvalidRegex(err) => format!("invalid regex in filter.title_regex: {err}"),
            Self::InvalidUuid(err) => format!("invalid UUID in filter.uuid: {err}"),
        }
    }
}

impl std::fmt::Display for QueryValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for QueryValidationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryEntriesResponse {
    pub entries: Vec<EntryPayload>,
    pub meta: QueryMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryPayload {
    pub uuid: String,
    pub title: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub url: Option<String>,
    pub notes: Option<String>,
    pub tags: Vec<String>,
    pub group_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryMeta {
    pub count: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeeGateInfoResponse {
    pub name: String,
    pub version: String,
    pub read_only: bool,
    pub authentication: String,
    pub query_features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeeGateApiErrorResponse {
    pub error: String,
    pub message: String,
}

pub fn startup_warnings(db: &StorageDatabase) -> Vec<String> {
    let Some(users_group) = users_group(db) else {
        return vec![format!(
            "KeeGate API enabled, but no root group named \"{USERS_GROUP_NAME}\" exists; API authentication will reject all users until the group is created"
        )];
    };

    let mut warnings = Vec::new();
    let mut usernames: BTreeMap<String, usize> = BTreeMap::new();

    for entry in &users_group.entries {
        let Some(username) = field_value(entry, "UserName") else {
            warnings.push(format!(
                "KeeGate API user entry '{}' is missing the UserName field and will be ignored",
                display_title(entry)
            ));
            continue;
        };
        let Some(_) = field_value(entry, "Password") else {
            warnings.push(format!(
                "KeeGate API user entry '{}' is missing the Password field and will be ignored",
                display_title(entry)
            ));
            continue;
        };

        *usernames.entry(username.to_string()).or_default() += 1;
    }

    for (username, count) in usernames {
        if count > 1 {
            warnings.push(format!(
                "KeeGate API username '{username}' appears multiple times in '{USERS_GROUP_NAME}' and will authenticate ambiguously"
            ));
        }
    }

    warnings
}

pub fn authenticate(
    db: &StorageDatabase,
    username: &str,
    password: &str,
) -> Result<AuthenticatedUser, AuthError> {
    let Some(users_group) = users_group(db) else {
        return Err(AuthError::InvalidCredentials);
    };

    let mut matching_entries = users_group
        .entries
        .iter()
        .filter_map(valid_user_entry)
        .filter(|candidate| candidate.username == username);

    let Some(entry) = matching_entries.next() else {
        return Err(AuthError::InvalidCredentials);
    };

    if matching_entries.next().is_some() {
        return Err(AuthError::AmbiguousUsername);
    }

    if entry.password != password {
        return Err(AuthError::InvalidCredentials);
    }

    Ok(AuthenticatedUser {
        username: entry.username.to_string(),
        tags: entry.entry.tags.iter().cloned().collect(),
    })
}

pub fn query_entries(
    db: &StorageDatabase,
    user: &AuthenticatedUser,
    query: &ValidatedQuery,
) -> QueryEntriesResponse {
    let mut entries = Vec::new();
    let mut group_path = Vec::new();
    collect_entries(&db.root, &mut group_path, &query.filter, user, &mut entries);

    entries.sort_by(|left, right| {
        let left_title = left.title.as_deref().unwrap_or("");
        let right_title = right.title.as_deref().unwrap_or("");
        left_title
            .cmp(right_title)
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    entries.truncate(query.limit);

    QueryEntriesResponse {
        meta: QueryMeta {
            count: entries.len(),
            limit: query.limit(),
        },
        entries,
    }
}

fn collect_entries(
    group: &StorageGroup,
    group_path: &mut Vec<String>,
    filter: &EntryFilter,
    user: &AuthenticatedUser,
    entries: &mut Vec<EntryPayload>,
) {
    for entry in &group.entries {
        if is_authorized(entry, user) && matches_filter(entry, filter) {
            entries.push(EntryPayload {
                uuid: entry.uuid.clone(),
                title: field_value(entry, "Title").map(ToOwned::to_owned),
                username: field_value(entry, "UserName").map(ToOwned::to_owned),
                password: field_value(entry, "Password").map(ToOwned::to_owned),
                url: field_value(entry, "URL").map(ToOwned::to_owned),
                notes: field_value(entry, "Notes").map(ToOwned::to_owned),
                tags: entry.tags.clone(),
                group_path: group_path.clone(),
            });
        }
    }

    for child in &group.groups {
        if group_path.is_empty() && child.name == USERS_GROUP_NAME {
            continue;
        }

        group_path.push(child.name.clone());
        collect_entries(child, group_path, filter, user, entries);
        group_path.pop();
    }
}

fn is_authorized(entry: &StorageEntry, user: &AuthenticatedUser) -> bool {
    !user.tags.is_empty() && entry.tags.iter().any(|tag| user.tags.contains(tag))
}

fn matches_filter(entry: &StorageEntry, filter: &EntryFilter) -> bool {
    match filter {
        EntryFilter::TitleContains(needle) => field_value(entry, "Title")
            .map(|title| title.to_lowercase().contains(needle))
            .unwrap_or(false),
        EntryFilter::TitleRegex(regex) => field_value(entry, "Title")
            .map(|title| regex.is_match(title))
            .unwrap_or(false),
        EntryFilter::Tag(tag) => entry.tags.iter().any(|entry_tag| entry_tag == tag),
        EntryFilter::Uuid(uuid) => Uuid::parse_str(&entry.uuid)
            .map(|entry_uuid| &entry_uuid == uuid)
            .unwrap_or(false),
        EntryFilter::And(filters) => filters.iter().all(|child| matches_filter(entry, child)),
        EntryFilter::Or(filters) => filters.iter().any(|child| matches_filter(entry, child)),
    }
}

fn users_group(db: &StorageDatabase) -> Option<&StorageGroup> {
    db.root
        .groups
        .iter()
        .find(|group| group.name == USERS_GROUP_NAME)
}

fn field_value<'a>(entry: &'a StorageEntry, key: &str) -> Option<&'a str> {
    entry.fields.get(key).map(|value| value.value.as_str())
}

fn display_title(entry: &StorageEntry) -> &str {
    field_value(entry, "Title").unwrap_or("<untitled>")
}

struct ValidUserEntry<'a> {
    entry: &'a StorageEntry,
    username: &'a str,
    password: &'a str,
}

fn valid_user_entry(entry: &StorageEntry) -> Option<ValidUserEntry<'_>> {
    Some(ValidUserEntry {
        entry,
        username: field_value(entry, "UserName")?,
        password: field_value(entry, "Password")?,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kdbx_git_storage_types::{
        StorageDatabase, StorageEntry, StorageGroup, StorageMeta, StorageTimes, StorageValue,
    };

    use super::{
        authenticate, query_entries, startup_warnings, AuthError, QueryEntriesRequest,
        USERS_GROUP_NAME,
    };

    fn sample_db() -> StorageDatabase {
        StorageDatabase {
            kdbx_config: Default::default(),
            meta: StorageMeta {
                generator: None,
                database_name: None,
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
                recyclebin_enabled: None,
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
            root: group("root", "Root"),
            deleted_objects: BTreeMap::new(),
        }
    }

    fn group(uuid: &str, name: &str) -> StorageGroup {
        StorageGroup {
            uuid: uuid.into(),
            name: name.into(),
            notes: None,
            icon_id: None,
            custom_icon: None,
            groups: vec![],
            entries: vec![],
            times: times(),
            custom_data: BTreeMap::new(),
            is_expanded: true,
            default_autotype_sequence: None,
            enable_autotype: None,
            enable_searching: None,
            last_top_visible_entry: None,
            tags: vec![],
            previous_parent_group: None,
        }
    }

    fn entry(
        uuid: &str,
        title: &str,
        username: &str,
        password: &str,
        tags: &[&str],
    ) -> StorageEntry {
        let mut fields = BTreeMap::new();
        fields.insert(
            "Title".into(),
            StorageValue {
                value: title.into(),
                protected: false,
            },
        );
        fields.insert(
            "UserName".into(),
            StorageValue {
                value: username.into(),
                protected: false,
            },
        );
        fields.insert(
            "Password".into(),
            StorageValue {
                value: password.into(),
                protected: true,
            },
        );

        StorageEntry {
            uuid: uuid.into(),
            fields,
            autotype: None,
            tags: tags.iter().map(|tag| (*tag).into()).collect(),
            times: times(),
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
        }
    }

    fn times() -> StorageTimes {
        StorageTimes {
            creation: None,
            last_modification: None,
            last_access: None,
            expiry: None,
            location_changed: None,
            expires: None,
            usage_count: None,
        }
    }

    #[test]
    fn startup_warns_when_users_group_is_missing() {
        let db = sample_db();
        let warnings = startup_warnings(&db);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(USERS_GROUP_NAME));
    }

    #[test]
    fn duplicate_usernames_authenticate_ambiguously() {
        let mut db = sample_db();
        let mut users = group("users", USERS_GROUP_NAME);
        users
            .entries
            .push(entry("user-1", "Alice A", "alice", "one", &["prod"]));
        users
            .entries
            .push(entry("user-2", "Alice B", "alice", "two", &["prod"]));
        db.root.groups.push(users);

        assert_eq!(
            authenticate(&db, "alice", "one").unwrap_err(),
            AuthError::AmbiguousUsername
        );
    }

    #[test]
    fn query_filters_and_authorizes_entries() {
        let mut db = sample_db();

        let mut users = group("users", USERS_GROUP_NAME);
        users
            .entries
            .push(entry("user-1", "Alice", "alice", "pw", &["prod"]));
        db.root.groups.push(users);

        let mut apps = group("apps", "Apps");
        let mut prod = entry(
            "2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e",
            "Prod Postgres",
            "db_admin",
            "secret",
            &["prod", "database"],
        );
        prod.fields.insert(
            "URL".into(),
            StorageValue {
                value: "https://db.example.com".into(),
                protected: false,
            },
        );
        apps.entries.push(prod);
        apps.entries.push(entry(
            "11111111-1111-1111-1111-111111111111",
            "Staging Redis",
            "cache",
            "secret",
            &["staging"],
        ));
        db.root.groups.push(apps);

        let user = authenticate(&db, "alice", "pw").unwrap();
        let request: QueryEntriesRequest = serde_json::from_str(
            r#"{
                "filter": {
                    "and": [
                        { "tag": "prod" },
                        { "title_regex": "(?i)postgres" }
                    ]
                }
            }"#,
        )
        .unwrap();
        let query = request.validate().unwrap();
        let response = query_entries(&db, &user, &query);

        assert_eq!(response.meta.count, 1);
        assert_eq!(response.entries[0].title.as_deref(), Some("Prod Postgres"));
        assert_eq!(response.entries[0].group_path, vec!["Apps".to_string()]);
        assert_eq!(
            response.entries[0].url.as_deref(),
            Some("https://db.example.com")
        );
    }
}
