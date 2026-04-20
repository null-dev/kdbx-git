# `keegate-client` Quick Start

`kdbx-git` stores a KeePass database in git and serves it through sync- and API-oriented tooling. KeeGate is the read-only HTTP API layer that lets applications fetch specific entry data, such as passwords or usernames, without needing direct access to the full `.kdbx` file.

`keegate-client` is the Rust client for that API. It can resolve `kg://...` references like `kg:///uuid/<uuid>` and `kg:///query?...`.

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
kdbx-git-keegate-client = { git = "https://github.com/null-dev/kdbx-git.git" }
```

Create a serde-friendly config struct in your app and embed `KeeGateClientConfig` inside it:

```rust
use kdbx_git_keegate_client::{KeeGateClient, KeeGateClientConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    keegate: KeeGateClientConfig,
}

let config = AppConfig {
    keegate: KeeGateClientConfig {
        url: "kg://username:password@host".into(),
    },
};

let client = KeeGateClient::from_config(&config.keegate)?;
```

Fetch entries with either structured queries or KeeGate URLs:

```rust
use kdbx_git_keegate_api::{QueryEntriesRequest, QueryFilterRequest, TagFilter};

let entries = client
    .query_entries(&QueryEntriesRequest {
        filter: QueryFilterRequest::Tag(TagFilter { tag: "prod".into() }),
        options: Default::default(),
    })
    .await?;

let password_entry = client
    .resolve_first("kg:///uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e")
    .await?;
```

Use `query_entries_get(...)` when you want simple query-string style search, and use `resolve(...)` / `resolve_first(...)` when your app stores KeeGate references like `kg:///uuid/...` or `kg:///query?...`.
