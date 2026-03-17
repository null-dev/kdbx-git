# E2E Test Checklist

Tests marked ✅ are already implemented. All others need to be written.

---

## WebDAV — Read path

- ✅ `init_imports_main_and_git_history_is_readable` — GET after `--init` returns the imported entries; git log shows `db.json`
- ✅ `client_writes_merge_and_fan_out_across_clients` — after alice writes, bob GET triggers merge-on-read and returns alice's entries; after bob writes, alice GET returns both entries
- ✅ GET when the client branch does not yet exist returns 404
- ✅ GET when only the client branch exists (no main) returns the client's own content
- ✅ GET after the client's own PUT returns that same content (round-trip identity)
- ✅ GET always includes content from main even when the client never wrote anything (merge-on-read via fast-forward)
- ✅ GET triggers merge-on-read: client branch behind main before GET, client branch contains main content after GET
- ✅ GET when merge-on-read fails (simulate corrupt main) still returns the client's stale data rather than an error
- ✅ GET on directory path (`/dav/{client_id}/`) returns the crate-native autoindex listing containing `database.kdbx`
- ✅ PROPFIND on `database.kdbx` returns 207 with Content-Length and Last-Modified properties
- ✅ PROPFIND on the root collection lists exactly one entry (`database.kdbx`)

## WebDAV — Write path

- ✅ `malformed_uploads_and_wrong_kdbx_password_are_rejected` — malformed body → 403; KDBX encrypted with wrong master password → 403; branch is not created
- ✅ PUT with valid KDBX creates the client branch and commits to main
- ✅ PUT with valid KDBX response status is 2xx (201 Created or 204 No Content)
- ✅ PUT creates a git commit with a meaningful message referencing the client id
- ✅ Second PUT with identical content does not create a new commit (no-op dedup)
- ✅ Second PUT with identical content does not fire an SSE event
- ✅ Second PUT with changed content does create a new commit and updates main
- ✅ PUT advances main only when the client → main merge succeeds
- ✅ PUT to a branch that is behind main still commits to the client branch and attempts merge to main
- ✅ Concurrent PUTs from two clients are serialised: both commits appear in main's history in some order
- ✅ PUT with an empty KDBX body (zero bytes) returns 403/400 and does not commit
- ✅ PUT followed immediately by GET returns the newly written content (write-then-read consistency)

## WebDAV — Auth

- ✅ `auth_failures_return_basic_auth_challenge` — no credentials → 401 with `WWW-Authenticate: Basic`; wrong password → 401
- [ ] Correct credentials for client A do not grant access to client B's DAV endpoint (cross-client isolation)
- [ ] Username belonging to client A with client B's password is rejected
- [ ] Credentials are case-sensitive (wrong-cased password → 401)
- [ ] Requests to unknown client paths (`/dav/nobody/database.kdbx`) return 401

## WebDAV — Data integrity / round-trip

- [ ] Entry with nested groups survives PUT → GET round-trip with structure intact
- [ ] Entry with custom fields survives round-trip
- [ ] Entry deletion: alice deletes an entry and PUTs; bob GETs and does not see the deleted entry (DeletedObjects respected)
- [ ] Entry modification: alice modifies a field and PUTs; bob GETs and sees the updated value
- [ ] Conflicting edits to the same entry UUID: alice and bob each modify the same entry independently; alice GETs after bob writes and receives a deterministically merged result
- [ ] Database name metadata survives round-trip

---

## sync-local — Pull (server → local file)

- ✅ `sync_local_creates_branch_and_pulls_from_main` — `--once` when alice's branch doesn't exist; local file is written with main's content
- ✅ `sync_local_updates_local_file_when_main_advances` — continuous mode; SSE event from bob's write causes alice's local file to be updated
- [ ] `--once` when alice is already up to date (204 from merge-from-main) exits cleanly without writing/modifying the local file
- [ ] `--once` when main does not exist exits cleanly and does not create the local file
- [ ] Continuous mode: multiple rapid SSE events (main advances several times quickly) — all updates eventually reach the local file
- [ ] After sync-local pulls, the local file is a valid KDBX that can be opened with the configured credentials
- [ ] After sync-local pulls, alice's branch on the server points at the promoted merge commit
- [ ] After sync-local promotes, a subsequent merge-from-main returns 204 (already up to date)
- [ ] Local file is written atomically: a crash observer never sees a partial/corrupt write (file is either old or new, never truncated)
- [ ] After pull, sync-local does not immediately push the file back (self-write suppression prevents an infinite loop)
- [ ] SSE reconnect: if the event stream drops, sync-local reconnects and resumes receiving updates

## sync-local — Push (local file → server)

- [ ] Modifying the local KDBX file causes sync-local to push it to the server via WebDAV PUT
- [ ] After a local push, main is updated on the server (PUT triggers client → main merge)
- [ ] After a local push, an SSE event fires, sync-local pulls the merged result back, and the local file is updated with the round-tripped content
- [ ] Pushing the local file with identical content (e.g. re-saved without changes) does not result in a server commit (server-side no-op dedup)
- [ ] Two sync-local instances (alice and bob): alice modifies her local file; bob's local file is eventually updated with alice's entry
- [ ] Rapid local saves (many writes in quick succession) are debounced into a single push, not a flood of PUT requests
- [ ] If the local file does not exist when a push would be triggered (e.g. deleted externally), sync-local does not error fatally
- [ ] Pre-existing local file on first start (server has no content for this client): file is pushed to the server

## sync-local — Interrupt recovery

- [ ] If the process exits after the local file is written but before promote completes, the state file (`*.sync-state.json`) contains the pending promote
- [ ] On the next startup, the pending promote is retried and completes successfully
- [ ] After recovering a pending promote, the state file is cleared
- [ ] If the pending promote fails with 409 on recovery (branch was modified externally), sync-local exits with a fatal error
- [ ] State file from a previous run with a stale commit ID (commit no longer accessible) produces a useful error, not a panic

## sync-local — Auth & error handling

- [ ] `/sync/{client_id}/events` with wrong credentials returns 401 and sync-local logs a warning
- [ ] `/sync/{client_id}/merge-from-main` with wrong credentials returns 401 and sync-local logs a warning
- [ ] Correct credentials for client A do not grant access to client B's sync endpoints
- [ ] `--once` mode exits after the initial reconcile, even if SSE events arrive before shutdown
- [ ] Unknown `client_id` in config returns a clear startup error, not a panic

---

## Sync API endpoints

- [ ] `POST /sync/{client_id}/merge-from-main` returns 204 when client branch already contains main
- [ ] `POST /sync/{client_id}/merge-from-main` returns 200 with KDBX body and `X-Merge-Commit-Id` / `X-Expected-Branch-Tip` headers when a merge is needed
- [ ] `POST /sync/{client_id}/merge-from-main` when main does not exist returns 204
- [ ] `POST /sync/{client_id}/merge-from-main` when client branch does not exist: returns merged content and `X-Expected-Branch-Tip: none`
- [ ] `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip=none` creates the client branch pointing at the commit
- [ ] `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip={hex}` advances the client branch when the tip matches
- [ ] `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip={hex}` returns 409 when the branch tip has changed since the merge was created
- [ ] `POST /sync/{client_id}/promote-merge/{bad_hex}` returns 400
- [ ] `POST /sync/{client_id}/promote-merge/{commit_id}?expected-tip={bad_hex}` returns 400
- [ ] SSE stream at `/sync/{client_id}/events` fires a `branch-updated` event when main advances after a WebDAV PUT
- [ ] SSE stream sends a `ready` event immediately on connection before any updates
- [ ] SSE stream does not fire when a WebDAV PUT results in a no-op (content unchanged)

---

## Multi-client / concurrency scenarios

- [ ] Three clients (alice, bob, carol): each writes a distinct entry; after all three writes, each client's GET returns all three entries
- [ ] Alice and bob write at the same time (goroutine/task race): server serialises both writes; after both finish, each client's GET sees both entries
- [ ] alice's sync-local is running while bob makes changes via WebDAV: alice's local file converges to include bob's entries within the timeout
- [ ] alice's sync-local is running while bob is also running sync-local: bob modifies his local file; alice's local file eventually receives bob's entries
- [ ] Client writes to branch, branch is subsequently promoted by sync-local to a merge commit: client's next GET still returns correct merged content
- [ ] Client with a branch far behind main (many commits behind) catches up in a single merge-from-main call
