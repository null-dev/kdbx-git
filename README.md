# kdbx-git

`kdbx-git` is a small sync server for KeePass databases that stores every revision in a bare git repository.

There are three main ways to use the server:

- through the WebDAV endpoint from KeePass clients that can open a remote database directly
- through the bundled `kdbx-git-sync-local` CLI, which keeps a local `.kdbx` file in sync with one client branch
- through the Android companion app, [kdbx-git Android](https://github.com/null-dev/kdbx-git-android), which exposes the synced database to Android KeePass apps through the Storage Access Framework

All three use the same per-client branch model on the server, so you can mix them freely across devices.

Each client gets its own branch and WebDAV credentials:

- client writes land on that client's branch first
- the server merges the client branch into `main`
- successful `main` updates are fanned back out to the other client branches
- successful `main` updates can also trigger UnifiedPush wakeups for registered mobile clients

The state of the KDBX database is stored unencrypted in the git store as pretty JSON by default so the history is readable with normal git tooling.

## Server Configuration

Create a `config.toml` like this:

```toml
git_store = "./store.git"
# sync_state_path = "./sync-state.json"
bind_addr = "0.0.0.0:8080"

[database]
password = "correct horse battery staple"
# keyfile = "./database.keyx"

[keegate_api]
enabled = true

[web_ui]
enabled = true
session_ttl_hours = 8

[[web_ui.admin_users]]
username = "admin"
password = "admin-password"

[[clients]]
id = "laptop"
password = "laptop-webdav-password"

[[clients]]
id = "phone"
password = "phone-webdav-password"
```

Notes:

- `database.password` / `database.keyfile` are the master credentials used to decrypt uploads and re-encrypt downloads.
- `git_store` is a bare repo, so inspect it by `cd`-ing into it and running normal git commands.
- `sync_state_path` optionally overrides where the server stores its sync state. If omitted, it defaults to `sync-state.json` next to `git_store`.
- `keegate_api.enabled` defaults to `true`; set it to `false` to disable the KeeGate HTTP API routes entirely.
- `web_ui.enabled` defaults to `false`; the dedicated `kdbx-git-web-ui` crate builds the Svelte frontend from `web-ui/` during Cargo builds, embeds the generated assets into the binary, and the server serves those embedded assets directly.
- `web_ui.admin_users` defines admin logins for the web UI using plaintext passwords from the config file.

## Usage

Import an existing KDBX file into the git store:

```bash
cargo run -p kdbx-git -- init --config config.toml ./seed.kdbx
```

Start the server:

```bash
cargo run -p kdbx-git -- --config config.toml
```

If you enable the web UI, `cargo build`, `cargo run`, and `cargo test -p kdbx-git` will build the `kdbx-git-web-ui` crate automatically. Then open:

```text
http://HOST:8080/ui
```

Once the server is running, you can connect to it in any of the following ways.

### 1. WebDAV clients

Point each client at its own WebDAV file:

- URL: `http://HOST:8080/dav/<client-id>/database.kdbx`
- username: the client `id`
- password: the matching client `password`

The database's master password/key file is still the KDBX master credential from the server config's `[database]` section.

### 2. Local file sync with `sync-local`

If you want `kdbx-git` to keep a normal local `.kdbx` file in sync for you, use the bundled `kdbx-git-sync-local` client.

Create a separate client config for each `sync-local` instance:

```toml
server_url = "http://127.0.0.1:8080"
client_id = "laptop"
password = "laptop-webdav-password"
# sync_state_path = "./laptop.sync-state.json"
```

Keep a local file in sync with a client branch through the running server:

```bash
cargo run -p kdbx-git-sync-local -- --config client.toml ./laptop.kdbx
```

Useful options:

- `--once`: perform a single reconciliation and exit
- `--poll`: also enable the local file polling probe for environments where filesystem notifications are unreliable

Examples:

```bash
# Pull or push once, then exit
cargo run -p kdbx-git-sync-local -- --config client.toml --once ./laptop.kdbx
```

`sync_state_path` optionally overrides where `kdbx-git-sync-local` stores its interrupt-recovery state JSON. If omitted, it defaults to `<local_path>.sync-state.json`.

### 3. Android mobile app

Android users can use the companion app, [kdbx-git Android](https://github.com/null-dev/kdbx-git-android), instead of mounting WebDAV directly.

- it syncs with the same server using the same client `id` and `password`
- it exposes the synced database as a local file through Android's Storage Access Framework
- it can receive instant updates via UnifiedPush, with FCM fallback when available

For Android-side setup details, see the app README: <https://github.com/null-dev/kdbx-git-android>.

## Instant Sync For Mobile Clients

Mobile clients can register a UnifiedPush endpoint with the server:

- `GET /push/<client-id>/vapid-public-key` to fetch the server's VAPID public key
- `POST /push/<client-id>/endpoint` with the full Web Push subscription JSON:

```json
{
  "endpoint": "https://push.example/...",
  "keys": {
    "p256dh": "...",
    "auth": "..."
  }
}
```

- `DELETE /push/<client-id>/endpoint` to unregister

These endpoints use the same HTTP Basic credentials as WebDAV:

- username: the client `id`
- password: the matching client `password`

After a successful write that advances `main`, the server sends a best-effort VAPID-signed
and encrypted web push payload to every registered subscription. Delivery runs in the
background with a short timeout, so the uploading client does not wait for push fan-out.

Endpoint registrations are stored in `sync-state.json`, pruned if they have not been
refreshed for 14 days, and removed automatically if the push provider responds with `404`
or `410`. The server also generates a VAPID keypair on startup and stores it in the same
state file if one does not already exist.

## Local File Sync Details

`kdbx-git-sync-local` keeps a local `.kdbx` file and a single client branch in sync through the server:

- it downloads/uploads through the same authenticated server endpoints the clients use
- remote branch changes are pushed to the CLI through a server event stream (`GET /sync/<client-id>/events`)
- local file changes are picked up from filesystem notifications by default, with optional polling fallback via `--poll`
- if both sides diverge, it runs the same KeePass-level merge logic and updates both sides

This is useful if you want branch-backed syncing without mounting WebDAV in your desktop workflow. The SSE event stream is used by `sync-local`; UnifiedPush is the instant-sync path intended for mobile clients.

## Inspecting the Password Store

Because the state is stored as pretty JSON in a bare git repo, you can inspect it directly with standard git commands — no server needed.

View the full history:

```bash
cd store.git
git log --stat main
```

Inspect the current database state of a specific client:

```bash
git show <client-name>:db.json
```

Compare a client's state against `main`:

```bash
git diff main <client-name> -- db.json
```

Show what changed in a particular commit:

```bash
git show <commit-hash>
```

## Docker

Build and run:

```bash
docker build -t kdbx-git .
docker run --rm -p 8080:8080 -v "$PWD:/data" kdbx-git --config /data/config.toml
```

If you are bootstrapping a fresh store inside the container, run:

```bash
docker run --rm -v "$PWD:/data" kdbx-git init --config /data/config.toml /data/seed.kdbx
```
