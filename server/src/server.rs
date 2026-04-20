//! Steps 7 & 8 — WebDAV server, HTTP Basic Auth, and concurrency control.
//!
//! # Architecture
//!
//! Each request is handled like this:
//!
//! 1. **Auth middleware** extracts the client ID from the URL path
//!    (`/dav/{client_id}/...`), validates Basic Auth credentials against the
//!    config, and stores the validated `client_id` in request extensions.
//!
//! 2. **`dav_handler`** retrieves the `client_id` from extensions, builds a
//!    per-request `KdbxFs` and a `DavHandler` with `strip_prefix` set to
//!    `/dav/{client_id}`, and delegates to dav-server.
//!
//! 3. **`KdbxFs`** implements `DavFileSystem` against a single virtual
//!    file `/database.kdbx`:
//!    - `metadata("/")` → root collection
//!    - `metadata("/database.kdbx")` → file exists iff the branch has commits
//!    - `open(read)` → merge main into the client branch, then build KDBX bytes
//!    - `open(write)` → accumulate bytes; on `flush`, decrypt and write to git
//!
//! 4. **`AppState`** wraps `GitStore` in `Arc<tokio::sync::Mutex<...>>`
//!    (step 8) so concurrent writes are serialised.

mod auth;
mod dav;
mod http;
mod keegate;
mod push;
mod state;
mod web_ui;

pub use http::{build_app, run_server, serve_listener};
pub use state::AppState;
