# Implementation Roadmap

## Step 1 — Project Scaffolding & Dependencies

- [x] Initialize the Rust binary crate (`src/main.rs`) inside the workspace.
- [x] Populate `Cargo.toml` with all required dependencies:
  - [x] `gix` (gitoxide) for git storage
  - [x] `keepass-nd` as a git dependency from `https://github.com/null-dev/keepass-nd`
  - [x] `serde` + `serde_json` / `serde_yaml` / `toml` for alternate formats (NUON skipped — its crate does not integrate with serde)
  - [x] `axum` for the HTTP layer
  - [x] `dav-server` (dav-server-rs) for WebDAV
  - [x] `eyre` + `color-eyre` for error handling
  - [x] `tracing` + `tracing-subscriber` (with `env-filter`) for logging
  - [x] Supporting crates: `tokio`, `uuid`, `base64`, `chrono`
- [x] Wire up `color_eyre` and `tracing_subscriber` in `main`.

---

## Step 2 — Configuration

Define and load server configuration (e.g., from a TOML file or env vars):

- [x] Path to the KDBX database file (used for initial import and as the encryption template).
- [x] Master password / key file for the KDBX database.
- [x] List of clients, each with:
  - [x] Unique client ID (becomes the branch name).
  - [x] WebDAV username & password.
- [x] HTTP bind address.
- [x] Path to the git storage directory.

---

## Step 3 — Database Serialization Layer

Define Rust structs that mirror the KDBX database content (groups, entries, metadata, etc.) and implement conversion to/from the keepass-nd model.

- [x] Implement `db_to_storage` / `storage_to_db` with JSON/YAML/TOML so database state can round-trip through text.
- [x] The on-disk format is one text file per commit stored in the git object store (indented for human-readability and `git diff` friendliness).
- [x] Write unit tests for round-trip fidelity.

---

## Step 4 — Git Storage Backend

Build a `GitStore` abstraction around gitoxide (`gix`):

- [x] **Initialize** a bare git repo on first run (or open an existing one).
- [x] **Read** a branch tip: deserialize the latest file blob on a branch into the in-memory database model.
- [x] **Write** a commit: serialize the in-memory model to text, create a tree + commit object, and advance the branch ref.
- [x] **Fast-forward check**: compare commit ancestry to decide if a merge can be skipped.
- [x] Keep operations `async`-friendly (run blocking gix calls inside `tokio::task::spawn_blocking`).

---

## Step 5 — Branch Management & Merge Logic

Implement the branch lifecycle used by the server:

- [ ] On first client access, create the client's branch by forking from `main` (or initializing it if `main` doesn't exist yet).
- [ ] **Client write flow**:
  1. Commit the new database state to the client's branch.
  2. Attempt to merge the client's branch into `main`:
     - If fast-forward is possible, just move `main`'s ref.
     - Otherwise, read both sides, call `keepass-nd`'s database merge method, and write the merged result as a new commit on `main`.
  3. If the merge to `main` succeeded, fan out: merge `main` into every other client branch using the same strategy.
- [ ] Conflict handling: if a merge produces an error, log it and leave the affected branches unchanged.

---

## Step 6 — KDBX Virtual File Construction

Implement the in-memory KDBX builder used for client reads:

- [ ] Read the client's branch tip from `GitStore`.
- [ ] Re-encrypt the database contents into a KDBX 4.1 binary using keepass-nd, using the same master credentials as the original database.
- [ ] Return the resulting bytes as the file body served over WebDAV.

---

## Step 7 — WebDAV Server

Wire up `dav-server-rs` as an Axum handler:

- [ ] Define a per-client route, e.g. `GET/PUT /dav/{client_id}/database.kdbx`.
- [ ] Implement a minimal `DavProvider` (or use `LocalFs` with a tmpfile strategy) that:
  - [ ] On `GET`: calls the virtual file builder from Step 6.
  - [ ] On `PUT`: receives the uploaded bytes, decrypts via keepass-nd, triggers the write flow from Step 5.
- [ ] Add HTTP Basic Auth middleware in Axum: extract the `Authorization` header, look up the client by credentials, and reject unknown clients with `401`.
- [ ] Serve all client routes under a single Axum `Router`.

---

## Step 8 — Concurrency & Locking

Ensure correctness under simultaneous client access:

- [ ] Wrap `GitStore` in an `Arc<tokio::sync::Mutex<GitStore>>` (or a per-branch `RwLock` map) so concurrent writes are serialized.
- [ ] Keep read operations (GET) outside the write lock where possible.
- [ ] Add tracing spans around every major operation for visibility.

---

## Step 9 — Integration Testing & Hardening

- [ ] Write an integration test that spins up the server in a temp directory, uploads a KDBX file as one client, reads it back as another, and verifies the merged content is consistent.
- [ ] Verify `git log` on the storage repo shows readable history (satisfying the "inspect with git CLI" requirement).
- [ ] Test the round-trip of all supported serialization formats (NUON, JSON, YAML, TOML) and pick one as the default.
- [ ] Harden error paths: malformed uploads, wrong password, ref update races.

---

## Step 10 — Polish & Packaging

- [ ] Add a `--init` subcommand to import an existing KDBX file and bootstrap the git repo + `main` branch.
- [ ] Write a concise `README.md` covering setup, config format, and how to point a KeePass client at the WebDAV endpoint.
- [ ] Add a `Dockerfile` for deployment.
