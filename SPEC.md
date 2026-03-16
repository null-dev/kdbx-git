## Program specification
I want to build a server that manages multiple clients accessing a single KDBX 4.1 database simultaneously.

- The software should store changes to the database using a git repo internally.
  - Use gitoxide (https://github.com/GitoxideLabs/gitoxide) to manage the git repo.
  - The git repo structure is mainly used due to it's:
    - It's good efficiency in storing many, many revisions of data that often changes
    - Ease of debugging/inspection of the storage format using external tools
 - Database content should be stored in the git repository in indented NUON/JSON/YAML/TOML format. NUON ser/de should be done using the nuon crate (https://crates.io/crates/nuon). JSON/YAML/TOML ser/de should be done using serde.
- Use my custom fork of the keepass library here: https://github.com/null-dev/keepass-nd to work with KDBX databases. It's not on crates.io, you'll need to pull it as a git dependency.
- Each client has a single branch, and there is also a "main" branch.
- All merges should be done using the keepass-nd library's database merge method (unless a fast-forward is possible).
- Each client's branch is exposed as a WebDAV endpoint that serves a virtual KDBX database file. Each client has it's own WebDAV credentials that it uses to access it's virtual database file.

### WebDAV behavior

- When a client reads their database file:
  - The server first attempts to merge the "main" branch into the client's branch (merge-on-read).
    - If this merge fails, the failure is logged as a warning and the client is served their current branch content without the merge.
  - A database file is dynamically constructed from the (possibly updated) client's branch and returned to the client.
- When a client writes their database file:
  - The new database file is decrypted and its contents written to the client's branch as a new commit.
  - The server then attempts to create a new commit on the "main" branch that merges the client's branch into "main".
    - If this merge into main fails, the client's commit is still kept on their branch and the client is told the write succeeded.
  - The server does NOT fan out changes from main into other client branches after a write. Each client's branch is only updated from main when that client reads (merge-on-read above).

### sync-local

`sync-local` is a client-side daemon that keeps a local KDBX file in sync with the server. It is pull-only: it never pushes local file changes to the server. All merging is performed server-side.

The sync-local client:
1. On startup, checks for any interrupted promote operation from a previous run (see step 5) and retries it.
2. Performs an initial reconcile by calling the server's merge-from-main endpoint.
3. Listens for SSE (Server-Sent Events) notifications that fire when the "main" branch advances.
4. On each notification (or on startup), calls `POST /sync/{client_id}/merge-from-main` to request a server-side merge of main into the client's branch. The server returns the merged KDBX bytes plus a commit ID and the client branch's expected tip.
5. Writes the new KDBX bytes to a temporary file, then atomically renames it to the local path.
6. Saves a "pending promote" record to a state file (`{local_path}.sync-state.json`) before calling step 7, so that an interruption can be recovered on next startup.
7. Calls `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip={tip}` to promote the temporary merge commit onto the client's branch on the server.
8. On a branch conflict error (409), the client exits with a fatal error (another process modified the branch unexpectedly). On other errors, the client logs a warning and continues.

The sync-local client never directly accesses the server's git database (which is unencrypted) — all git operations happen server-side.

#### Server-side sync endpoints

- `POST /sync/{client_id}/merge-from-main`
  - Creates a temporary merge commit (stored at `refs/sync-temp/{client_id}`) that merges the main branch into the client's branch.
  - If the client is already at or ahead of main (no merge needed), returns 204 No Content.
  - Otherwise returns 200 with the merged KDBX bytes in the body, and headers:
    - `X-Merge-Commit-Id`: the object ID of the temporary merge commit
    - `X-Expected-Branch-Tip`: the object ID the client's branch currently points to (used for CAS in promote)
  - Uses a fast-forward optimization: if the client branch is an ancestor of main (or does not yet exist), the temp ref is pointed directly at main's tip rather than creating a new merge commit.

- `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip={tip|none}`
  - Promotes a previously created temporary merge commit onto the client's branch.
  - Uses compare-and-swap: only succeeds if the client's branch tip matches `expected-tip`. Returns 409 Conflict if the branch was modified by another process since the merge was created.
  - Returns 200 on success.

### Other dependencies
- Use eyre + color_eyre for error handling
- Use axum for serving HTTP
- WebDAV handling: https://github.com/messense/dav-server-rs
- For logging/observability, use: tracing, tracing-subscriber (with env-filter)

### Other notes
- It should be possible to inspect the storage using the regular git CLI
