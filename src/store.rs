//! Git-backed database store (steps 4 & 5).
//!
//! # Design
//!
//! [`GitStore`] wraps a bare gitoxide [`gix::ThreadSafeRepository`].  Every
//! blocking gix call is executed inside `tokio::task::spawn_blocking` so the
//! async runtime is never stalled.
//!
//! ## Branch layout
//!
//! ```text
//! refs/heads/main          ← canonical merged state
//! refs/heads/<client-id>   ← per-client branch
//! ```
//!
//! Each commit contains exactly one file (`db.json` / `db.yaml` / `db.toml`)
//! that holds the full database snapshot serialised by [`crate::storage`].

use crate::storage::{
    convert::{db_to_storage, storage_to_db},
    format::{deserialize, serialize, StorageFormat},
    types::StorageDatabase,
};
use eyre::{bail, Context, Result};
use gix::ObjectId;
use keepass::config::{
    CompressionConfig, DatabaseConfig, DatabaseVersion, InnerCipherConfig, KdfConfig,
    OuterCipherConfig,
};
use std::path::Path;
use tokio::task::spawn_blocking;
use tracing::{debug, info, warn};

// ── Constants ────────────────────────────────────────────────────────────────

pub const MAIN_BRANCH: &str = "main";
const BOT_NAME: &str = "kdbx-git";
const BOT_EMAIL: &str = "kdbx-git@localhost";
const REF_UPDATE_RETRIES: usize = 3;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// A minimal DatabaseConfig used only for in-memory merge operations.
/// The cipher/KDF values only matter when saving KDBX files (step 6).
fn merge_db_config() -> DatabaseConfig {
    DatabaseConfig {
        version: DatabaseVersion::KDB4(1), // KDBX 4.1
        outer_cipher_config: OuterCipherConfig::AES256,
        compression_config: CompressionConfig::GZip,
        inner_cipher_config: InnerCipherConfig::ChaCha20,
        kdf_config: KdfConfig::Aes { rounds: 6000 },
        public_custom_data: None,
    }
}

fn bot_signature() -> gix::actor::Signature {
    gix::actor::Signature {
        name: gix::bstr::BString::from(BOT_NAME),
        email: gix::bstr::BString::from(BOT_EMAIL),
        time: gix::date::Time::now_local_or_utc(),
    }
}

// ── GitStore ──────────────────────────────────────────────────────────────────

pub struct GitStore {
    repo: gix::ThreadSafeRepository,
    format: StorageFormat,
}

impl GitStore {
    /// Open an existing bare git repository at `path`, or initialise a new one.
    pub fn open_or_init(path: &Path) -> Result<Self> {
        let repo = if path.join("HEAD").exists() {
            gix::open(path).wrap_err("failed to open git store")?
        } else {
            std::fs::create_dir_all(path).wrap_err("failed to create git store directory")?;
            gix::init_bare(path).wrap_err("failed to initialise git store")?
        };
        Ok(Self {
            repo: repo.into_sync(),
            format: StorageFormat::Json,
        })
    }

    // ── Sync core (executed inside spawn_blocking) ────────────────────────────

    /// Read the storage database from the tip of `branch`.
    fn read_branch_sync(
        repo: &gix::Repository,
        branch: &str,
        format: StorageFormat,
    ) -> Result<Option<StorageDatabase>> {
        let ref_name = format!("refs/heads/{branch}");
        let reference = match repo
            .try_find_reference(ref_name.as_str())
            .wrap_err_with(|| format!("failed to look up ref {ref_name}"))?
        {
            Some(r) => r,
            None => return Ok(None),
        };

        let commit_id = reference
            .try_id()
            .ok_or_else(|| eyre::eyre!("ref {ref_name} is symbolic, expected direct"))?
            .detach();

        let tree_id = repo
            .find_object(commit_id)
            .wrap_err("commit object not found")?
            .try_into_commit()
            .wrap_err("object is not a commit")?
            .tree_id()
            .wrap_err("commit has no tree")?
            .detach();

        let tree = repo
            .find_object(tree_id)
            .wrap_err("tree object not found")?
            .try_into_tree()
            .wrap_err("object is not a tree")?;

        let decoded = tree.decode().wrap_err("failed to decode tree")?;
        let file_name = format.file_name();

        for entry in &decoded.entries {
            if entry.filename == file_name.as_bytes() {
                let blob_id = entry.oid.to_owned();
                let blob = repo
                    .find_object(blob_id)
                    .wrap_err("blob object not found")?
                    .try_into_blob()
                    .wrap_err("tree entry is not a blob")?;
                let text = std::str::from_utf8(blob.data.as_ref())
                    .wrap_err("database blob is not valid UTF-8")?;
                return deserialize(text, format).map(Some);
            }
        }

        bail!(
            "file '{}' not found in tree for branch '{branch}'",
            file_name
        );
    }

    /// Return the OID at the tip of `branch`, or `None` if it doesn't exist.
    fn branch_tip_id_sync(repo: &gix::Repository, branch: &str) -> Result<Option<ObjectId>> {
        let ref_name = format!("refs/heads/{branch}");
        match repo
            .try_find_reference(ref_name.as_str())
            .wrap_err_with(|| format!("failed to look up ref {ref_name}"))?
        {
            Some(r) => Ok(Some(
                r.try_id()
                    .ok_or_else(|| eyre::eyre!("ref {ref_name} is symbolic"))?
                    .detach(),
            )),
            None => Ok(None),
        }
    }

    /// Atomically update `branch` to point at `new_commit`.
    ///
    /// `prev_commit` must exactly match the current tip (for new branches, pass `None`).
    fn set_branch_ref_sync(
        repo: &gix::Repository,
        branch: &str,
        new_commit: ObjectId,
        prev_commit: Option<ObjectId>,
        message: &str,
    ) -> Result<()> {
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        use gix::refs::Target;

        let expected = match prev_commit {
            Some(id) => PreviousValue::MustExistAndMatch(Target::Object(id)),
            None => PreviousValue::MustNotExist,
        };

        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: message.into(),
                },
                expected,
                new: Target::Object(new_commit),
            },
            name: format!("refs/heads/{branch}")
                .try_into()
                .wrap_err("invalid branch name")?,
            deref: false,
        };

        repo.edit_references([edit])
            .wrap_err("failed to update branch ref")?;
        Ok(())
    }

    /// Write `storage` as a new commit on `branch` and return the new commit OID.
    fn commit_to_branch_sync(
        repo: &gix::Repository,
        branch: &str,
        storage: &StorageDatabase,
        format: StorageFormat,
        message: &str,
    ) -> Result<ObjectId> {
        let text = serialize(storage, format)?;

        for attempt in 0..REF_UPDATE_RETRIES {
            // blob
            let blob_id = repo
                .write_blob(text.as_bytes())
                .wrap_err("failed to write blob")?
                .detach();

            // tree (single entry)
            let entry = gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: format.file_name().into(),
                oid: blob_id,
            };
            let tree_obj = gix::objs::Tree {
                entries: vec![entry],
            };
            let tree_id = repo
                .write_object(&tree_obj)
                .wrap_err("failed to write tree")?
                .detach();

            let parent = Self::branch_tip_id_sync(repo, branch)?;

            let sig = bot_signature();
            let commit_obj = gix::objs::Commit {
                tree: tree_id,
                parents: parent.into_iter().collect::<Vec<_>>().into(),
                author: sig.clone(),
                committer: sig,
                encoding: None,
                message: message.into(),
                extra_headers: vec![],
            };
            let commit_id = repo
                .write_object(&commit_obj)
                .wrap_err("failed to write commit")?
                .detach();

            match Self::set_branch_ref_sync(repo, branch, commit_id, parent, message) {
                Ok(()) => return Ok(commit_id),
                Err(err) => {
                    let observed = Self::branch_tip_id_sync(repo, branch)?;
                    let raced = observed != parent;
                    if raced && attempt + 1 < REF_UPDATE_RETRIES {
                        warn!(
                            "ref update raced while committing to '{branch}', retrying (attempt {}/{})",
                            attempt + 2,
                            REF_UPDATE_RETRIES
                        );
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        unreachable!("ref update retry loop must return or error")
    }

    /// Return `true` if `ancestor` is a reachable ancestor of `descendant`
    /// (or they are the same commit).
    fn is_ancestor_sync(
        repo: &gix::Repository,
        ancestor: ObjectId,
        descendant: ObjectId,
    ) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        match repo.merge_base(ancestor, descendant) {
            Ok(base) => Ok(base == ancestor),
            // No common ancestor → definitely not an ancestor
            Err(_) => Ok(false),
        }
    }

    // ── Async public API (step 4) ─────────────────────────────────────────────

    /// Read the storage database from the tip of `branch`.
    /// Returns `None` if the branch does not exist yet.
    pub async fn read_branch(&self, branch: String) -> Result<Option<StorageDatabase>> {
        let repo = self.repo.clone();
        let format = self.format;
        spawn_blocking(move || Self::read_branch_sync(&repo.to_thread_local(), &branch, format))
            .await
            .wrap_err("blocking task panicked")?
    }

    /// Commit `storage` to `branch` and return the new commit OID.
    pub async fn commit_to_branch(
        &self,
        branch: String,
        storage: StorageDatabase,
        message: String,
    ) -> Result<ObjectId> {
        let repo = self.repo.clone();
        let format = self.format;
        spawn_blocking(move || {
            Self::commit_to_branch_sync(
                &repo.to_thread_local(),
                &branch,
                &storage,
                format,
                &message,
            )
        })
        .await
        .wrap_err("blocking task panicked")?
    }

    /// Return the tip OID for `branch`, or `None` if the branch doesn't exist.
    pub async fn branch_tip_id(&self, branch: String) -> Result<Option<ObjectId>> {
        let repo = self.repo.clone();
        spawn_blocking(move || Self::branch_tip_id_sync(&repo.to_thread_local(), &branch))
            .await
            .wrap_err("blocking task panicked")?
    }

    /// Import an initial database state onto `main`.
    pub async fn bootstrap_main(
        &self,
        storage: StorageDatabase,
        message: String,
    ) -> Result<ObjectId> {
        if self.branch_tip_id(MAIN_BRANCH.to_string()).await?.is_some() {
            bail!("{MAIN_BRANCH} already exists; refusing to overwrite imported history");
        }

        self.commit_to_branch(MAIN_BRANCH.to_string(), storage, message)
            .await
    }

    /// Return `true` if `ancestor` is reachable from `descendant`.
    pub async fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        let repo = self.repo.clone();
        spawn_blocking(move || {
            Self::is_ancestor_sync(&repo.to_thread_local(), ancestor, descendant)
        })
        .await
        .wrap_err("blocking task panicked")?
    }

    /// Move `branch` to point at `to`, used for fast-forward operations.
    async fn fast_forward_branch(
        &self,
        branch: String,
        to: ObjectId,
        message: String,
    ) -> Result<()> {
        let repo = self.repo.clone();
        spawn_blocking(move || {
            let repo = repo.to_thread_local();
            for attempt in 0..REF_UPDATE_RETRIES {
                let prev = Self::branch_tip_id_sync(&repo, &branch)?;
                if prev == Some(to) {
                    return Ok(());
                }

                match Self::set_branch_ref_sync(&repo, &branch, to, prev, &message) {
                    Ok(()) => return Ok(()),
                    Err(err) => {
                        let observed = Self::branch_tip_id_sync(&repo, &branch)?;
                        let raced = observed != prev;
                        if raced && attempt + 1 < REF_UPDATE_RETRIES {
                            warn!(
                                "ref update raced while fast-forwarding '{branch}', retrying (attempt {}/{})",
                                attempt + 2,
                                REF_UPDATE_RETRIES
                            );
                            continue;
                        }
                        return Err(err);
                    }
                }
            }

            unreachable!("ref update retry loop must return or error")
        })
        .await
        .wrap_err("blocking task panicked")?
    }

    // ── Async branch management (step 5) ─────────────────────────────────────

    /// Ensure the client branch exists.
    ///
    /// If `main` exists the branch is created as an alias of its current tip
    /// (zero-cost fork), so the first merge will always be a fast-forward.
    /// If neither branch exists the client branch will be created on its first
    /// write (via [`Self::commit_to_branch`]).
    pub async fn ensure_client_branch(&self, client_branch: String) -> Result<()> {
        if self.branch_tip_id(client_branch.clone()).await?.is_some() {
            return Ok(());
        }
        if let Some(main_tip) = self.branch_tip_id(MAIN_BRANCH.to_string()).await? {
            info!("Creating branch '{client_branch}' from {MAIN_BRANCH} @ {main_tip}");
            self.fast_forward_branch(client_branch, main_tip, format!("fork from {MAIN_BRANCH}"))
                .await?;
        }
        Ok(())
    }

    /// Merge `from_branch` into `into_branch`.
    ///
    /// - If `into_branch` is an ancestor of `from_branch` (or identical), a
    ///   fast-forward ref update is performed.
    /// - If `from_branch` is already an ancestor of `into_branch`, there is
    ///   nothing to do and `false` is returned.
    /// - Otherwise the keepass merge algorithm is used and the result is
    ///   committed to `into_branch`.
    ///
    /// Returns `true` when `into_branch` was actually updated.
    pub async fn merge_branch_into(
        &self,
        from_branch: String,
        into_branch: String,
    ) -> Result<bool> {
        let from_tip = match self.branch_tip_id(from_branch.clone()).await? {
            Some(id) => id,
            None => return Ok(false),
        };

        let into_tip = self.branch_tip_id(into_branch.clone()).await?;

        // ── into_branch doesn't exist yet — just point it at from_tip ────────
        if into_tip.is_none() {
            info!("Creating '{into_branch}' pointing to '{from_branch}' @ {from_tip}");
            self.fast_forward_branch(
                into_branch,
                from_tip,
                format!("initial: from '{from_branch}'"),
            )
            .await?;
            return Ok(true);
        }

        // ── Fast-forward or already-up-to-date check ─────────────────────────
        if let Some(into_id) = into_tip {
            if self.is_ancestor(into_id, from_tip).await? {
                // into is strictly behind from → fast-forward
                info!("Fast-forwarding '{into_branch}' to '{from_branch}' @ {from_tip}");
                self.fast_forward_branch(
                    into_branch.clone(),
                    from_tip,
                    format!("fast-forward from '{from_branch}'"),
                )
                .await?;
                return Ok(true);
            }
            if self.is_ancestor(from_tip, into_id).await? {
                // from is already contained in into — nothing to do
                debug!("'{into_branch}' already contains '{from_branch}', skipping merge");
                return Ok(false);
            }
        }

        // ── Keepass merge ─────────────────────────────────────────────────────
        let from_storage = self
            .read_branch(from_branch.clone())
            .await?
            .ok_or_else(|| eyre::eyre!("branch '{from_branch}' has no content"))?;

        let merged = match self.read_branch(into_branch.clone()).await? {
            Some(into_storage) => {
                let from = from_branch.clone();
                let into = into_branch.clone();
                spawn_blocking(move || {
                    merge_databases(&into_storage, &from_storage)
                        .wrap_err_with(|| format!("merge of '{from}' into '{into}' failed"))
                })
                .await
                .wrap_err("merge task panicked")??
            }
            // into_branch has no commits yet — use from as-is
            None => from_storage,
        };

        let msg = format!("merge '{from_branch}' into '{into_branch}'");
        self.commit_to_branch(into_branch.clone(), merged, msg)
            .await?;
        info!("Merged '{from_branch}' into '{into_branch}'");
        Ok(true)
    }

    /// Execute the full client-write flow (spec step 2–3):
    ///
    /// 1. Commit `storage` to `client_branch`.
    /// 2. Merge `client_branch` → `main`.
    /// 3. If that succeeded, merge `main` → every other client branch.
    pub async fn process_client_write(
        &self,
        client_branch: String,
        storage: StorageDatabase,
        all_client_branches: Vec<String>,
    ) -> Result<Vec<String>> {
        let mut updated_branches = vec![client_branch.clone()];

        // 1. Commit new state to client's own branch
        let msg = format!("write from client '{client_branch}'");
        self.commit_to_branch(client_branch.clone(), storage, msg)
            .await?;
        info!("Committed to client branch '{client_branch}'");

        // 2. Merge client → main
        let merged_main = self
            .merge_branch_into(client_branch.clone(), MAIN_BRANCH.to_string())
            .await
            .unwrap_or_else(|e| {
                warn!("Failed to merge '{client_branch}' into {MAIN_BRANCH}: {e:#}");
                false
            });

        if !merged_main {
            return Ok(updated_branches);
        }
        updated_branches.push(MAIN_BRANCH.to_string());

        // 3. Fan out: merge main → all other client branches
        for other in all_client_branches
            .iter()
            .filter(|b| b.as_str() != client_branch.as_str())
        {
            match self
                .merge_branch_into(MAIN_BRANCH.to_string(), other.clone())
                .await
            {
                Ok(true) => updated_branches.push(other.clone()),
                Ok(false) => {}
                Err(e) => warn!("Failed to merge {MAIN_BRANCH} into '{other}': {e:#}"),
            }
        }

        Ok(updated_branches)
    }
}

/// Perform a keepass-level merge of `from_storage` into `into_storage`.
pub fn merge_databases(
    into_storage: &StorageDatabase,
    from_storage: &StorageDatabase,
) -> Result<StorageDatabase> {
    let config = merge_db_config();
    let mut into_db = storage_to_db(into_storage, config.clone())
        .wrap_err("failed to reconstruct 'into' database for merge")?;
    let from_db = storage_to_db(from_storage, config)
        .wrap_err("failed to reconstruct 'from' database for merge")?;
    into_db
        .merge(&from_db)
        .map_err(|e| eyre::eyre!("keepass merge failed: {e:?}"))?;
    db_to_storage(&into_db).wrap_err("failed to convert merged database back to storage")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::types::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_temp_store() -> (TempDir, GitStore) {
        let dir = TempDir::new().unwrap();
        let store = GitStore::open_or_init(dir.path()).unwrap();
        (dir, store)
    }

    fn simple_db(name: &str) -> StorageDatabase {
        StorageDatabase {
            meta: StorageMeta {
                generator: Some("kdbx-git-test".into()),
                database_name: Some(name.into()),
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
                                value: name.into(),
                                protected: false,
                            },
                        );
                        m.insert(
                            "Password".into(),
                            StorageValue {
                                value: "hunter2".into(),
                                protected: true,
                            },
                        );
                        m
                    },
                    autotype: None,
                    tags: vec![],
                    times: StorageTimes {
                        creation: Some("2024-01-01T00:00:00".into()),
                        last_modification: Some("2024-01-01T00:00:01".into()),
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

    // ── Step 4 tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_init_and_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let _store = GitStore::open_or_init(dir.path()).unwrap();
        }
        // Should be able to reopen without error
        let _store2 = GitStore::open_or_init(dir.path()).unwrap();
    }

    #[tokio::test]
    async fn test_write_and_read_branch() {
        let (_dir, store) = make_temp_store();
        let db = simple_db("TestDB");

        // Branch doesn't exist yet
        assert!(store.read_branch("main".into()).await.unwrap().is_none());

        // Write it
        store
            .commit_to_branch("main".into(), db.clone(), "initial".into())
            .await
            .unwrap();

        // Read it back
        let read = store.read_branch("main".into()).await.unwrap().unwrap();
        assert_eq!(read.meta.database_name, Some("TestDB".into()));
        assert_eq!(read.root.entries[0].fields["Password"].value, "hunter2");
        assert!(read.root.entries[0].fields["Password"].protected);
    }

    #[tokio::test]
    async fn test_second_write_appends_commit() {
        let (_dir, store) = make_temp_store();

        store
            .commit_to_branch("main".into(), simple_db("v1"), "first".into())
            .await
            .unwrap();
        let tip1 = store.branch_tip_id("main".into()).await.unwrap().unwrap();

        store
            .commit_to_branch("main".into(), simple_db("v2"), "second".into())
            .await
            .unwrap();
        let tip2 = store.branch_tip_id("main".into()).await.unwrap().unwrap();

        assert_ne!(tip1, tip2);

        let read = store.read_branch("main".into()).await.unwrap().unwrap();
        assert_eq!(read.meta.database_name, Some("v2".into()));
    }

    #[tokio::test]
    async fn test_is_ancestor() {
        let (_dir, store) = make_temp_store();

        store
            .commit_to_branch("main".into(), simple_db("v1"), "c1".into())
            .await
            .unwrap();
        let c1 = store.branch_tip_id("main".into()).await.unwrap().unwrap();

        store
            .commit_to_branch("main".into(), simple_db("v2"), "c2".into())
            .await
            .unwrap();
        let c2 = store.branch_tip_id("main".into()).await.unwrap().unwrap();

        assert!(store.is_ancestor(c1, c2).await.unwrap());
        assert!(!store.is_ancestor(c2, c1).await.unwrap());
        assert!(store.is_ancestor(c1, c1).await.unwrap());
    }

    // ── Step 5 tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ensure_client_branch_no_main() {
        let (_dir, store) = make_temp_store();
        // Should succeed silently when main doesn't exist yet
        store.ensure_client_branch("alice".into()).await.unwrap();
        // No branch created yet (will happen on first write)
        assert!(store.branch_tip_id("alice".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_ensure_client_branch_forks_main() {
        let (_dir, store) = make_temp_store();

        store
            .commit_to_branch("main".into(), simple_db("main-db"), "init main".into())
            .await
            .unwrap();
        let main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();

        store.ensure_client_branch("alice".into()).await.unwrap();

        let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
        assert_eq!(
            alice_tip, main_tip,
            "alice should start at the same commit as main"
        );
    }

    #[tokio::test]
    async fn test_fast_forward_merge() {
        // Client writes to an empty store; main should be fast-forwarded.
        let (_dir, store) = make_temp_store();

        store
            .process_client_write("alice".into(), simple_db("alice-db"), vec!["alice".into()])
            .await
            .unwrap();

        // main should now point to alice's commit
        let alice_tip = store.branch_tip_id("alice".into()).await.unwrap().unwrap();
        let main_tip = store.branch_tip_id("main".into()).await.unwrap().unwrap();
        assert_eq!(
            alice_tip, main_tip,
            "main should be fast-forwarded to alice's commit"
        );
    }

    #[tokio::test]
    async fn test_bootstrap_main_rejects_overwrite() {
        let (_dir, store) = make_temp_store();

        store
            .bootstrap_main(simple_db("initial"), "import".into())
            .await
            .unwrap();

        let err = store
            .bootstrap_main(simple_db("second"), "import again".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
    }

    #[tokio::test]
    async fn test_two_clients_write_and_merge() {
        let (_dir, store) = make_temp_store();

        // Alice writes first
        store
            .process_client_write(
                "alice".into(),
                simple_db("alice-db"),
                vec!["alice".into(), "bob".into()],
            )
            .await
            .unwrap();

        // Ensure bob's branch exists (forked from main == alice's commit)
        store.ensure_client_branch("bob".into()).await.unwrap();

        // Bob writes independently (diverging from alice's commit)
        let mut bob_db = simple_db("bob-db");
        // Add a second entry so the databases genuinely diverge
        bob_db.root.entries.push(StorageEntry {
            uuid: "00000000-0000-0000-0000-000000000099".into(),
            fields: {
                let mut m = BTreeMap::new();
                m.insert(
                    "Title".into(),
                    StorageValue {
                        value: "Bob's entry".into(),
                        protected: false,
                    },
                );
                m
            },
            autotype: None,
            tags: vec![],
            times: StorageTimes {
                creation: Some("2024-01-02T00:00:00".into()),
                last_modification: Some("2024-01-02T00:00:01".into()),
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
        });

        store
            .process_client_write("bob".into(), bob_db, vec!["alice".into(), "bob".into()])
            .await
            .unwrap();

        // Alice's branch should have received the merged main (which includes Bob's entry)
        let alice_db = store.read_branch("alice".into()).await.unwrap().unwrap();
        let titles: Vec<_> = alice_db
            .root
            .entries
            .iter()
            .filter_map(|e| e.fields.get("Title"))
            .map(|v| v.value.as_str())
            .collect();
        assert!(
            titles.contains(&"Bob's entry"),
            "Alice should have received Bob's entry after merge; got: {titles:?}"
        );
    }
}
