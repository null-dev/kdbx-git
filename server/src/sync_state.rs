use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use eyre::{Context, Result};
use jwt_simple::prelude::ES256KeyPair;
use serde::{Deserialize, Serialize};
use web_push::VapidSignatureBuilder;

const SYNC_STATE_FILE_NAME: &str = "sync-state.json";
const TEMP_FILE_SUFFIX: &str = ".tmp";
const PUSH_ENDPOINT_TTL_DAYS: i64 = 14;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PushEndpointRecord {
    pub endpoint: String,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncState {
    #[serde(default)]
    pub vapid: Option<VapidKeys>,
    #[serde(default)]
    pub push_endpoints: BTreeMap<String, PushEndpointRecord>,
}

impl SyncState {
    fn prune_expired(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(PUSH_ENDPOINT_TTL_DAYS);
        self.push_endpoints
            .retain(|_, record| record.last_seen_at >= cutoff);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevokedPushEndpoint {
    pub client_id: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VapidKeys {
    pub private_key: String,
    pub public_key: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SyncStateStore {
    path: PathBuf,
}

impl SyncStateStore {
    pub(crate) fn for_git_store(git_store: &Path) -> Self {
        let parent = git_store
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            path: parent.join(SYNC_STATE_FILE_NAME),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<SyncState> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => {
                serde_json::from_str(&contents).wrap_err("failed to parse sync-state.json")
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(SyncState::default()),
            Err(err) => Err(err).wrap_err("failed to read sync-state.json"),
        }
    }

    pub(crate) fn upsert_push_endpoint(
        &self,
        client_id: &str,
        endpoint: String,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut state = self.load()?;
        state.prune_expired(now);
        state.push_endpoints.insert(
            client_id.to_string(),
            PushEndpointRecord {
                endpoint,
                last_seen_at: now,
            },
        );
        self.save(&mut state, now)
    }

    pub(crate) fn remove_push_endpoint(&self, client_id: &str, now: DateTime<Utc>) -> Result<()> {
        let mut state = self.load()?;
        state.prune_expired(now);
        state.push_endpoints.remove(client_id);
        self.save(&mut state, now)
    }

    pub(crate) fn remove_revoked_push_endpoints(
        &self,
        revoked: &[RevokedPushEndpoint],
        now: DateTime<Utc>,
    ) -> Result<()> {
        if revoked.is_empty() {
            return Ok(());
        }

        let mut state = self.load()?;
        state.prune_expired(now);

        for revoked_entry in revoked {
            let should_remove = state
                .push_endpoints
                .get(&revoked_entry.client_id)
                .map(|current| current.endpoint == revoked_entry.endpoint)
                .unwrap_or(false);

            if should_remove {
                state.push_endpoints.remove(&revoked_entry.client_id);
            }
        }

        self.save(&mut state, now)
    }

    pub(crate) fn ensure_vapid_keys(&self) -> Result<VapidKeys> {
        let now = Utc::now();
        let mut state = self.load()?;
        if let Some(vapid) = state.vapid.clone() {
            return Ok(vapid);
        }

        let vapid = generate_vapid_keys();
        state.vapid = Some(vapid.clone());
        self.save(&mut state, now)?;
        Ok(vapid)
    }

    fn save(&self, state: &mut SyncState, now: DateTime<Utc>) -> Result<()> {
        state.prune_expired(now);

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).wrap_err("failed to create sync-state directory")?;
        }

        let temp_name = format!(
            "{}{}",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(SYNC_STATE_FILE_NAME),
            TEMP_FILE_SUFFIX
        );
        let temp_path = self.path.with_file_name(temp_name);
        let bytes =
            serde_json::to_vec_pretty(state).wrap_err("failed to serialize sync-state.json")?;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .wrap_err("failed to open temporary sync-state.json file")?;
        file.write_all(&bytes)
            .wrap_err("failed to write temporary sync-state.json file")?;
        file.write_all(b"\n")
            .wrap_err("failed to finalize temporary sync-state.json file")?;
        file.sync_all()
            .wrap_err("failed to fsync temporary sync-state.json file")?;
        drop(file);

        std::fs::rename(&temp_path, &self.path)
            .wrap_err("failed to atomically replace sync-state.json")?;
        Ok(())
    }
}

fn generate_vapid_keys() -> VapidKeys {
    let key_pair = ES256KeyPair::generate();
    let private_key = URL_SAFE_NO_PAD.encode(key_pair.to_bytes());
    let public_key = URL_SAFE_NO_PAD.encode(
        VapidSignatureBuilder::from_base64_no_sub(&private_key)
            .expect("generated VAPID private key should be valid")
            .get_public_key(),
    );

    VapidKeys {
        private_key,
        public_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    #[test]
    fn save_prunes_expired_entries_and_keeps_recent_ones() {
        let tempdir = TempDir::new().unwrap();
        let store = SyncStateStore::new(tempdir.path().join("sync-state.json"));
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap();

        let mut state = SyncState::default();
        state.push_endpoints.insert(
            "expired".into(),
            PushEndpointRecord {
                endpoint: "https://push.example/expired".into(),
                last_seen_at: now - Duration::days(15),
            },
        );
        state.push_endpoints.insert(
            "fresh".into(),
            PushEndpointRecord {
                endpoint: "https://push.example/fresh".into(),
                last_seen_at: now - Duration::days(3),
            },
        );

        store.save(&mut state, now).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.push_endpoints.len(), 1);
        assert_eq!(
            loaded.push_endpoints["fresh"].endpoint,
            "https://push.example/fresh"
        );
    }

    #[test]
    fn remove_revoked_push_endpoints_only_removes_matching_endpoint() {
        let tempdir = TempDir::new().unwrap();
        let store = SyncStateStore::new(tempdir.path().join("sync-state.json"));
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap();

        store
            .upsert_push_endpoint("alice", "https://push.example/old".into(), now)
            .unwrap();
        store
            .upsert_push_endpoint("alice", "https://push.example/new".into(), now)
            .unwrap();

        store
            .remove_revoked_push_endpoints(
                &[RevokedPushEndpoint {
                    client_id: "alice".into(),
                    endpoint: "https://push.example/old".into(),
                }],
                now,
            )
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.push_endpoints.len(), 1);
        assert_eq!(
            loaded.push_endpoints["alice"].endpoint,
            "https://push.example/new"
        );
    }

    #[test]
    fn save_uses_temp_file_and_cleans_it_up() {
        let tempdir = TempDir::new().unwrap();
        let store = SyncStateStore::new(tempdir.path().join("sync-state.json"));
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap();

        store
            .upsert_push_endpoint("alice", "https://push.example/alice".into(), now)
            .unwrap();

        assert!(store.path().exists());
        assert!(!store.path().with_file_name("sync-state.json.tmp").exists());
    }

    #[test]
    fn ensure_vapid_keys_creates_and_persists_keypair() {
        let tempdir = TempDir::new().unwrap();
        let store = SyncStateStore::new(tempdir.path().join("sync-state.json"));

        let generated = store.ensure_vapid_keys().unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.vapid, Some(generated.clone()));
        assert!(!generated.private_key.is_empty());
        assert!(!generated.public_key.is_empty());
    }

    #[test]
    fn ensure_vapid_keys_reuses_existing_keypair() {
        let tempdir = TempDir::new().unwrap();
        let store = SyncStateStore::new(tempdir.path().join("sync-state.json"));
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap();
        let existing = VapidKeys {
            private_key: "private".into(),
            public_key: "public".into(),
        };

        let mut state = SyncState::default();
        state.vapid = Some(existing.clone());
        store.save(&mut state, now).unwrap();

        let ensured = store.ensure_vapid_keys().unwrap();
        assert_eq!(ensured, existing);
    }
}
