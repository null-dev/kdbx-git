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
//! refs/sync-temp/<client-id> ← temporary merge commit awaiting promotion
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
const SYNC_TEMP_REF_PREFIX: &str = "refs/sync-temp/";

// ── Public types ──────────────────────────────────────────────────────────────

/// Returned by [`GitStore::create_sync_merge_commit`].
pub struct SyncMergeResult {
    /// The merged database, ready to be serialised and written locally.
    pub storage: StorageDatabase,
    /// The OID of the temporary merge commit stored in `refs/sync-temp/<branch>`.
    pub commit_id: ObjectId,
    /// The branch tip that was current when the merge was created.
    /// `None` means the client branch did not exist yet.
    /// Must be passed back to [`GitStore::promote_sync_merge_commit`] so it can
    /// detect unexpected concurrent modifications.
    pub expected_branch_tip: Option<ObjectId>,
}

/// Returned by [`GitStore::promote_sync_merge_commit`] when the client branch
/// was modified by a third party between `create` and `promote`.
#[derive(Debug)]
pub struct BranchConflictError {
    pub branch: String,
}

impl std::fmt::Display for BranchConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "branch '{}' was modified unexpectedly; aborting promote",
            self.branch
        )
    }
}

impl std::error::Error for BranchConflictError {}

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

    /// Read the raw serialised text of the database file at the tip of `branch`.
    ///
    /// Returns `None` if the branch does not exist.
    fn read_branch_text_sync(
        repo: &gix::Repository,
        branch: &str,
        format: StorageFormat,
    ) -> Result<Option<String>> {
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
                    .wrap_err("database blob is not valid UTF-8")?
                    .to_owned();
                return Ok(Some(text));
            }
        }

        bail!(
            "file '{}' not found in tree for branch '{branch}'",
            file_name
        );
    }

    /// Read the storage database from the tip of `branch`.
    fn read_branch_sync(
        repo: &gix::Repository,
        branch: &str,
        format: StorageFormat,
    ) -> Result<Option<StorageDatabase>> {
        match Self::read_branch_text_sync(repo, branch, format)? {
            Some(text) => deserialize(&text, format).map(Some),
            None => Ok(None),
        }
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

    /// Atomically update a ref given by its full name.
    ///
    /// `prev_commit` must exactly match the current value (for new refs, pass `None`).
    fn set_ref_sync(
        repo: &gix::Repository,
        full_ref_name: &str,
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
            name: full_ref_name
                .try_into()
                .wrap_err("invalid ref name")?,
            deref: false,
        };

        repo.edit_references([edit])
            .wrap_err("failed to update ref")?;
        Ok(())
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
        Self::set_ref_sync(
            repo,
            &format!("refs/heads/{branch}"),
            new_commit,
            prev_commit,
            message,
        )
    }

    /// Overwrite an arbitrary ref (create or replace, any previous value).
    fn force_set_ref_sync(
        repo: &gix::Repository,
        full_ref_name: &str,
        new_commit: ObjectId,
        message: &str,
    ) -> Result<()> {
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        use gix::refs::Target;

        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: message.into(),
                },
                expected: PreviousValue::Any,
                new: Target::Object(new_commit),
            },
            name: full_ref_name
                .try_into()
                .wrap_err("invalid ref name")?,
            deref: false,
        };

        repo.edit_references([edit])
            .wrap_err("failed to force-set ref")?;
        Ok(())
    }

    /// Delete a ref (no-op if it does not exist).
    fn delete_ref_sync(repo: &gix::Repository, full_ref_name: &str) -> Result<()> {
        use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};

        let edit = RefEdit {
            change: Change::Delete {
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name: full_ref_name
                .try_into()
                .wrap_err("invalid ref name")?,
            deref: false,
        };

        repo.edit_references([edit])
            .wrap_err("failed to delete ref")?;
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

    /// Merge `main` into `client_branch` (persistent).  Called on WebDAV reads
    /// so the client always sees the latest merged state.  Failures are
    /// non-fatal — the caller should log a warning and serve the unmerged data.
    pub async fn merge_main_into_branch(&self, client_branch: String) -> Result<bool> {
        self.merge_branch_into(MAIN_BRANCH.to_string(), client_branch)
            .await
    }

    /// Execute the client-write flow:
    ///
    /// 1. Commit `storage` to `client_branch`.
    /// 2. Merge `client_branch` → `main`.
    ///
    /// Fan-out (main → other clients) is intentionally omitted; clients pull
    /// from main at read time via [`Self::merge_main_into_branch`].
    ///
    /// Returns the list of branch names that were actually updated.
    pub async fn process_client_write(
        &self,
        client_branch: String,
        storage: StorageDatabase,
    ) -> Result<Vec<String>> {
        // Check whether the incoming content differs from the current tip. If
        // not, skip the commit entirely to avoid polluting the git history.
        let unchanged = {
            let repo = self.repo.clone();
            let format = self.format;
            let branch = client_branch.clone();
            let new_storage = storage.clone();
            spawn_blocking(move || -> Result<bool> {
                let repo = repo.to_thread_local();
                let new_text = serialize(&new_storage, format)?;
                match Self::read_branch_text_sync(&repo, &branch, format)? {
                    Some(existing_text) => Ok(new_text == existing_text),
                    None => Ok(false),
                }
            })
            .await
            .wrap_err("blocking task panicked")??
        };

        if unchanged {
            info!("Client '{}': content unchanged, skipping commit", client_branch);
            return Ok(vec![]);
        }

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

        if merged_main {
            updated_branches.push(MAIN_BRANCH.to_string());
        }

        Ok(updated_branches)
    }

    // ── Sync-local server-side merge API ─────────────────────────────────────

    /// Create a temporary merge commit that merges `main` into `client_branch`.
    ///
    /// The commit is written to `refs/sync-temp/<client_branch>` and is NOT
    /// yet part of the client branch history.  Call
    /// [`Self::promote_sync_merge_commit`] after the local KDBX file has been
    /// written to finalise the operation.
    ///
    /// Returns `None` when `main` does not exist or the client branch already
    /// contains `main` (nothing to do).
    pub async fn create_sync_merge_commit(
        &self,
        client_branch: String,
    ) -> Result<Option<SyncMergeResult>> {
        let repo = self.repo.clone();
        let format = self.format;
        spawn_blocking(move || {
            Self::create_sync_merge_commit_sync(&repo.to_thread_local(), &client_branch, format)
        })
        .await
        .wrap_err("blocking task panicked")?
    }

    fn create_sync_merge_commit_sync(
        repo: &gix::Repository,
        client_branch: &str,
        format: StorageFormat,
    ) -> Result<Option<SyncMergeResult>> {
        let main_tip = match Self::branch_tip_id_sync(repo, MAIN_BRANCH)? {
            Some(id) => id,
            None => return Ok(None), // main doesn't exist
        };

        let client_tip = Self::branch_tip_id_sync(repo, client_branch)?;

        // If client already contains main, nothing to do.
        if let Some(cid) = client_tip {
            if Self::is_ancestor_sync(repo, main_tip, cid)? {
                return Ok(None);
            }
        }

        // Compute merged storage.
        let main_storage = Self::read_branch_sync(repo, MAIN_BRANCH, format)?
            .ok_or_else(|| eyre::eyre!("main branch has no content"))?;

        // If the client branch is an ancestor of main (or doesn't exist), use
        // main's content directly — no keepass merge needed.
        let is_fast_forward = match client_tip {
            Some(cid) => Self::is_ancestor_sync(repo, cid, main_tip)?,
            None => true,
        };

        let merged = if is_fast_forward {
            main_storage
        } else {
            match Self::read_branch_sync(repo, client_branch, format)? {
                Some(client_storage) => {
                    merge_databases(&client_storage, &main_storage)
                        .wrap_err("failed to merge main into client branch")?
                }
                None => main_storage,
            }
        };

        // Determine the commit ID to promote.
        //
        // Fast-forward: point the temp ref directly at main_tip — no new commit
        // needed.  This preserves the ancestry chain so the *next* reconcile can
        // also fast-forward instead of falling back to a keepass merge.
        //
        // Real merge: create a new merge commit with two parents so that the
        // full history of both branches is reachable from alice's branch after
        // promotion.
        let commit_id = if is_fast_forward {
            main_tip
        } else {
            let text = serialize(&merged, format)?;
            let blob_id = repo
                .write_blob(text.as_bytes())
                .wrap_err("failed to write blob")?
                .detach();

            let entry = gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: format.file_name().into(),
                oid: blob_id,
            };
            let tree_id = repo
                .write_object(&gix::objs::Tree {
                    entries: vec![entry],
                })
                .wrap_err("failed to write tree")?
                .detach();

            let sig = bot_signature();
            // Two parents: client tip (first) and main tip (second).
            let parents: Vec<ObjectId> = client_tip.into_iter().chain([main_tip]).collect();
            repo.write_object(&gix::objs::Commit {
                tree: tree_id,
                parents: parents.into(),
                author: sig.clone(),
                committer: sig,
                encoding: None,
                message: format!("sync: merge {MAIN_BRANCH} into '{client_branch}'").into(),
                extra_headers: vec![],
            })
            .wrap_err("failed to write merge commit")?
            .detach()
        };

        // Store in the sync-temp ref (overwrite any previous one).
        let temp_ref = format!("{SYNC_TEMP_REF_PREFIX}{client_branch}");
        Self::force_set_ref_sync(repo, &temp_ref, commit_id, "sync-temp")?;

        info!(
            "Created sync temp ref for '{client_branch}' (commit: {commit_id}, expected tip: {client_tip:?})"
        );

        Ok(Some(SyncMergeResult {
            storage: merged,
            commit_id,
            expected_branch_tip: client_tip,
        }))
    }

    /// Promote the temporary merge commit created by
    /// [`Self::create_sync_merge_commit`] onto `client_branch`.
    ///
    /// `merge_commit_id` must match the OID stored in the sync-temp ref.
    /// `expected_branch_tip` must match the current tip of `client_branch`
    /// (i.e. it must not have been modified since the merge commit was created).
    ///
    /// Returns [`BranchConflictError`] (wrapped in [`eyre::Report`]) when the
    /// branch has been modified unexpectedly so the caller can distinguish this
    /// from transient failures and exit immediately.
    pub async fn promote_sync_merge_commit(
        &self,
        client_branch: String,
        merge_commit_id: ObjectId,
        expected_branch_tip: Option<ObjectId>,
    ) -> Result<()> {
        let repo = self.repo.clone();
        spawn_blocking(move || {
            Self::promote_sync_merge_commit_sync(
                &repo.to_thread_local(),
                &client_branch,
                merge_commit_id,
                expected_branch_tip,
            )
        })
        .await
        .wrap_err("blocking task panicked")?
    }

    fn promote_sync_merge_commit_sync(
        repo: &gix::Repository,
        client_branch: &str,
        merge_commit_id: ObjectId,
        expected_branch_tip: Option<ObjectId>,
    ) -> Result<()> {
        // Verify the temp ref matches the requested commit.
        let temp_ref = format!("{SYNC_TEMP_REF_PREFIX}{client_branch}");
        let temp_tip = match repo
            .try_find_reference(temp_ref.as_str())
            .wrap_err("failed to look up sync-temp ref")?
        {
            Some(r) => r
                .try_id()
                .ok_or_else(|| eyre::eyre!("sync-temp ref is symbolic"))?
                .detach(),
            None => bail!("no pending sync merge commit found for '{client_branch}'"),
        };

        if temp_tip != merge_commit_id {
            bail!(
                "sync-temp ref for '{client_branch}' points to {temp_tip}, not {merge_commit_id}"
            );
        }

        // Verify the branch has not been touched since the merge was created.
        let current_tip = Self::branch_tip_id_sync(repo, client_branch)?;
        if current_tip != expected_branch_tip {
            return Err(eyre::Report::new(BranchConflictError {
                branch: client_branch.to_string(),
            }));
        }

        // Atomically advance the client branch.
        Self::set_branch_ref_sync(
            repo,
            client_branch,
            merge_commit_id,
            current_tip,
            "sync: promote merge commit",
        )?;

        // Clean up the temp ref.
        Self::delete_ref_sync(repo, &temp_ref)?;

        info!("Promoted sync merge commit {merge_commit_id} onto '{client_branch}'");
        Ok(())
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
    async fn test_process_client_write_skips_commit_when_unchanged() {
        let (_dir, store) = make_temp_store();

        // Initial write — should create a commit.
        let updated = store
            .process_client_write("alice".into(), simple_db("Alice DB"))
            .await
            .unwrap();
        assert!(!updated.is_empty(), "first write should update branches");
        let tip_after_first = store
            .branch_tip_id("alice".into())
            .await
            .unwrap()
            .unwrap();

        // Second write with identical content — should be a no-op.
        let updated2 = store
            .process_client_write("alice".into(), simple_db("Alice DB"))
            .await
            .unwrap();
        assert!(updated2.is_empty(), "unchanged write should update no branches");
        let tip_after_second = store
            .branch_tip_id("alice".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tip_after_first, tip_after_second,
            "branch tip should not advance on unchanged write"
        );

        // Third write with different content — should commit.
        let updated3 = store
            .process_client_write("alice".into(), simple_db("Alice DB v2"))
            .await
            .unwrap();
        assert!(!updated3.is_empty(), "changed write should update branches");
        let tip_after_third = store
            .branch_tip_id("alice".into())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            tip_after_second, tip_after_third,
            "branch tip should advance on changed write"
        );
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
            .process_client_write("alice".into(), simple_db("alice-db"))
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
    async fn test_two_clients_write_no_fanout() {
        // With the new spec, client writes no longer fan out to other branches.
        let (_dir, store) = make_temp_store();

        // Alice writes first
        store
            .process_client_write("alice".into(), simple_db("alice-db"))
            .await
            .unwrap();

        // Ensure bob's branch exists (forked from main == alice's commit)
        store.ensure_client_branch("bob".into()).await.unwrap();

        // Bob writes independently (diverging from alice's commit)
        let mut bob_db = simple_db("bob-db");
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
            .process_client_write("bob".into(), bob_db)
            .await
            .unwrap();

        // Main should have Bob's entry (merged from bob's branch).
        let main_db = store.read_branch("main".into()).await.unwrap().unwrap();
        let main_titles: Vec<_> = main_db
            .root
            .entries
            .iter()
            .filter_map(|e| e.fields.get("Title"))
            .map(|v| v.value.as_str())
            .collect();
        assert!(
            main_titles.contains(&"Bob's entry"),
            "main should have Bob's entry after merge; got: {main_titles:?}"
        );

        // Alice's branch should NOT have Bob's entry (no fanout).
        let alice_db = store.read_branch("alice".into()).await.unwrap().unwrap();
        let alice_titles: Vec<_> = alice_db
            .root
            .entries
            .iter()
            .filter_map(|e| e.fields.get("Title"))
            .map(|v| v.value.as_str())
            .collect();
        assert!(
            !alice_titles.contains(&"Bob's entry"),
            "alice's branch should NOT have Bob's entry (no fanout); got: {alice_titles:?}"
        );
    }

    #[tokio::test]
    async fn test_merge_main_into_branch_updates_client() {
        let (_dir, store) = make_temp_store();

        // Alice writes first (main == alice's commit)
        store
            .process_client_write("alice".into(), simple_db("alice-db"))
            .await
            .unwrap();

        // Bob is forked from main
        store.ensure_client_branch("bob".into()).await.unwrap();

        // Alice writes again → main is updated, bob's branch is NOT updated (no fanout)
        store
            .process_client_write("alice".into(), simple_db("alice-db-v2"))
            .await
            .unwrap();

        // Bob's branch should still be at the old commit
        let bob_db = store.read_branch("bob".into()).await.unwrap().unwrap();
        assert_eq!(bob_db.meta.database_name, Some("alice-db".into()));

        // Simulate bob's WebDAV read: merge main into bob's branch
        let updated = store
            .merge_main_into_branch("bob".into())
            .await
            .unwrap();
        assert!(updated, "bob's branch should have been updated");

        let bob_db = store.read_branch("bob".into()).await.unwrap().unwrap();
        assert_eq!(bob_db.meta.database_name, Some("alice-db-v2".into()));
    }

    #[tokio::test]
    async fn test_create_and_promote_sync_merge_commit() {
        let (_dir, store) = make_temp_store();

        // Set up initial state: main has alice's write, bob is forked.
        store
            .process_client_write("alice".into(), simple_db("initial"))
            .await
            .unwrap();
        store.ensure_client_branch("bob".into()).await.unwrap();

        // Alice writes again → main advances but bob's branch is NOT updated.
        store
            .process_client_write("alice".into(), simple_db("updated"))
            .await
            .unwrap();

        // sync-local for bob: create the merge commit.
        let result = store
            .create_sync_merge_commit("bob".into())
            .await
            .unwrap()
            .expect("should have something to merge");

        assert_eq!(result.storage.meta.database_name, Some("updated".into()));

        // Promote it.
        store
            .promote_sync_merge_commit(
                "bob".into(),
                result.commit_id,
                result.expected_branch_tip,
            )
            .await
            .unwrap();

        // Bob's branch should now be up to date.
        let bob_db = store.read_branch("bob".into()).await.unwrap().unwrap();
        assert_eq!(bob_db.meta.database_name, Some("updated".into()));
    }

    #[tokio::test]
    async fn test_promote_detects_branch_conflict() {
        let (_dir, store) = make_temp_store();

        store
            .process_client_write("alice".into(), simple_db("v1"))
            .await
            .unwrap();
        store.ensure_client_branch("bob".into()).await.unwrap();
        store
            .process_client_write("alice".into(), simple_db("v2"))
            .await
            .unwrap();

        // Create the merge commit for bob.
        let result = store
            .create_sync_merge_commit("bob".into())
            .await
            .unwrap()
            .unwrap();

        // Simulate bob's branch being modified externally before promotion.
        store
            .commit_to_branch("bob".into(), simple_db("external"), "external write".into())
            .await
            .unwrap();

        // Promotion should fail with BranchConflictError.
        let err = store
            .promote_sync_merge_commit(
                "bob".into(),
                result.commit_id,
                result.expected_branch_tip,
            )
            .await
            .unwrap_err();

        assert!(
            err.downcast_ref::<BranchConflictError>().is_some(),
            "expected BranchConflictError, got: {err:#}"
        );
    }
}
