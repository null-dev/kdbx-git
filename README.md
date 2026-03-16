# kdbx-git

`kdbx-git` is a small WebDAV server and local sync tool that lets multiple KeePass clients share one KDBX database while storing every revision in a bare git repository.

Each client gets its own branch and WebDAV credentials:

- client writes land on that client's branch first
- the server merges the client branch into `main`
- successful `main` updates are fanned back out to the other client branches

The git store uses pretty JSON by default so the history stays readable with normal git tooling.

## Configuration

Create a `config.toml` like this:

```toml
git_store = "./store.git"
bind_addr = "0.0.0.0:8080"

[database]
path = "./seed.kdbx"
password = "correct horse battery staple"
# keyfile = "./database.keyx"

[[clients]]
id = "laptop"
username = "laptop"
password = "laptop-webdav-password"

[[clients]]
id = "phone"
username = "phone"
password = "phone-webdav-password"
```

Notes:

- `database.path` is required for `--init` and points at the existing KDBX file to import.
- `database.password` / `database.keyfile` are the master credentials used to decrypt uploads and re-encrypt downloads.
- `git_store` is a bare repo, so inspect it with commands like `git --git-dir ./store.git log --stat main`.

## Usage

Import an existing KDBX file into the git store:

```bash
cargo run -- --init config.toml
```

Start the server:

```bash
cargo run -- config.toml
```

Keep a local file in sync with a client branch through the running server:

```bash
cargo run -- sync-local config.toml laptop ./laptop.kdbx
```

Useful options:

- `--once`: perform a single reconciliation and exit
- `--server-url URL`: connect to a different running server URL instead of inferring one from `bind_addr`

Examples:

```bash
# Pull or push once, then exit
cargo run -- sync-local --once config.toml laptop ./laptop.kdbx

# Connect to a separately hosted server instance
cargo run -- sync-local --server-url https://vault.example.com config.toml laptop ./laptop.kdbx
```

## KeePass Client Setup

Point each client at its own WebDAV file:

- URL: `http://HOST:8080/dav/<client-id>/database.kdbx`
- username: the matching client `username`
- password: the matching client `password`

The database's master password/key file is still the KDBX master credential from the `[database]` section.

## Local File Sync

`sync-local` keeps a local `.kdbx` file and a single client branch in sync through the server:

- it downloads/uploads through the same authenticated server endpoints the clients use
- remote branch changes are pushed to the CLI through a server event stream
- local file changes are picked up from filesystem notifications, with a small safety-net detector to catch missed updates
- if both sides diverge, it runs the same KeePass-level merge logic and updates both sides

This is useful if you want branch-backed syncing without mounting WebDAV in your desktop workflow.

## Docker

Build and run:

```bash
docker build -t kdbx-git .
docker run --rm -p 8080:8080 -v "$PWD:/data" kdbx-git /data/config.toml
```

If you are bootstrapping a fresh store inside the container, run:

```bash
docker run --rm -v "$PWD:/data" kdbx-git --init /data/config.toml
```
