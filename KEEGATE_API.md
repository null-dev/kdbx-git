# KeeGate HTTP API Plan

## Goal

Add a read-only HTTP API that lets external applications retrieve passwords and related entry metadata from the server-managed KeePass database without exposing the full database file.

This API should:

- authenticate clients with HTTP Basic Auth
- authorize access based on KeePass data inside the database itself
- support a KeeGate URL format that combines host, username, and password in one connection string
- allow clients to resolve one or more entries from a single `kg://...` string without constructing JSON queries
- allow entry lookup by title substring, title regex, tag, and UUID
- support combining multiple search predicates with `and` and `or`
- return only entries the authenticated user is allowed to access

## Non-Goals

This initial API should not:

- create, update, or delete entries
- expose attachments or entry history
- replace the existing WebDAV or sync flows
- introduce a second external user store outside the KeePass database

## High-Level Design

The server will expose a new versioned API namespace:

- `/api/v1/keegate/...`

The API will be served by the existing `axum` server alongside:

- `/dav/...`
- `/sync/...`
- `/push/...`

The API is read-only in the first version. Every API request authenticates with HTTP Basic Auth. The username/password are resolved against a special root-level group in the KeePass database named exactly:

- `KeeGate Users`

Each entry directly inside `KeeGate Users` defines one API user:

- KeePass entry username field: API username
- KeePass entry password field: API password
- KeePass entry tags: authorization tags for that user

The user may access any other entry in the database whose tags intersect with the user's tags.

### Separate Authentication Domain

KeeGate API auth must be fully separate from the server's existing HTTP auth used for WebDAV, sync, and push endpoints.

That means:

- no reuse of the existing path-derived client auth flow
- no reuse of `config.toml` client credentials
- no requirement that an API username match any configured server client ID
- no path structure like `/api/<client-id>/...`
- no shared auth middleware or auth helper that assumes WebDAV client semantics

The only source of truth for KeeGate API authentication is the `KeeGate Users` group inside the database.

## KeeGate URL Scheme

Clients should support a KeeGate-specific URL scheme that packages connection details and, optionally, an entry locator into a single string.

Supported forms in v1:

- base connection string: `kg://username:password@host`
- absolute UUID reference: `kg://username:password@host/uuid/<uuid>`
- absolute query reference: `kg://username:password@host/query?...`
- config-relative UUID reference: `kg:///uuid/<uuid>`
- config-relative query reference: `kg:///query?...`

Notes:

- `host` may include a port, for example `kg://alice:secret@example.com:8443`
- if the username or password contains reserved URL characters, it must use normal percent-encoding
- v1 defines `/uuid/<uuid>` and `/query?...` path forms inside `kg://` URLs
- `kg://` is a client-side convenience format; the server still exposes normal HTTPS endpoints
- clients should assume a resolved `kg://` URL may return multiple entries
- if a client only expects one entry, it should use the first returned entry after resolution

### Client Config

Clients should allow the KeeGate connection string to be configured once:

```toml
[keegate]
url = "kg://username:password@host"
```

This client configuration should be standardized across implementations so a KeeGate API client can be constructed directly from configuration JSON when the host application already stores config in a JSON-compatible format.

Suggested logical shape:

```json
{
  "keegate": {
    "url": "kg://username:password@host"
  }
}
```

Rust note:

- Rust clients do not need to accept an untyped JSON blob as their constructor input
- instead, the KeeGate client config should be represented as a dedicated struct that is `serde` serializable and deserializable
- Rust applications should embed that struct inside their own top-level config structs rather than inventing a separate ad hoc config shape

Then any setting or UI field that needs a KeeGate-backed secret can accept either:

- a config-relative reference such as `kg:///uuid/<uuid>` or `kg:///query?...`
- a fully qualified override such as `kg://username:password@host2/uuid/<uuid>`

Resolution rules:

1. If the value includes credentials and host, use that exact authority.
2. If the value is `kg:///...`, load the authority from `[keegate].url`.
3. Append the resolved path to `/api/v1/keegate/entries/resolve`.
4. Convert the request to HTTPS before sending it to the server.
5. Treat the response as a list of entries, even if the calling UI only needs one secret.

If `kg:///...` is used but `[keegate].url` is missing or invalid, the client should fail locally before making any HTTP request.

Example transformations:

- `kg://username:password@host/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`
  becomes
  `https://username:password@host/api/v1/keegate/entries/resolve/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`
- with `[keegate].url = "kg://username:password@host"`, `kg:///uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`
  becomes
  `https://username:password@host/api/v1/keegate/entries/resolve/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`
- with `[keegate].url = "kg://username:password@host"`, `kg:///query?tag=prod`
  becomes
  `https://username:password@host/api/v1/keegate/entries/resolve/query?tag=prod`

Authentication semantics do not change: the resolved HTTPS request still authenticates as standard HTTP Basic Auth against `KeeGate Users`.

## Access Control Model

### User Discovery

On each API request, the server reads the current database state and locates a root group named exactly `KeeGate Users`.

Rules:

- only a direct child of the root group counts
- only entries directly inside `KeeGate Users` are treated as API users
- nested groups inside `KeeGate Users` are ignored in v1
- the `KeeGate Users` group itself is never returned by search results
- entries inside `KeeGate Users` are never returned by search results

### Startup Behavior

At startup:

- if `KeeGate Users` does not exist, the server logs a warning
- the server still starts successfully

Suggested warning text:

```text
KeeGate API enabled, but no root group named "KeeGate Users" exists; API authentication will reject all users until the group is created
```

### Authentication

The API uses standard HTTP Basic Auth:

- `Authorization: Basic <base64(username:password)>`

Authentication flow:

1. Decode the Basic Auth credentials.
2. Load the current database snapshot.
3. Find the root group `KeeGate Users`.
4. Find a direct child entry whose username field matches the Basic Auth username.
5. Compare the entry password field with the Basic Auth password.
6. If they match, authenticate as that KeeGate user.
7. Otherwise return `401 Unauthorized`.

Notes:

- if multiple `KeeGate Users` entries share the same username, authentication should fail closed with `401 Unauthorized` and log a warning
- if the username or password field is missing on a user entry, that entry is ignored and a warning should be logged
- if `KeeGate Users` does not exist, every API auth attempt returns `401 Unauthorized`
- this authentication flow is independent from WebDAV client auth even if both use the HTTP Basic Auth header format

### Authorization

After authentication, collect all tags from the authenticated user entry.

Access rule:

- a database entry is visible if it has at least one tag that exactly matches at least one tag on the authenticated user entry

Additional rules:

- matching is exact and case-sensitive in v1
- users with zero tags can authenticate, but can access no entries
- untagged entries are not accessible through the API
- access is evaluated per entry after search matching
- the authenticated user entry itself is never accessible as a normal result

## API Surface

### 1. Health / Capability Endpoint

`GET /api/v1/keegate/info`

Purpose:

- confirm that the API is enabled
- advertise the API version and supported query features

Authentication:

- no auth required

Example response:

```json
{
  "name": "KeeGate API",
  "version": "v1",
  "read_only": true,
  "authentication": "basic",
  "query_features": [
    "title_contains",
    "title_regex",
    "tag",
    "uuid",
    "and",
    "or"
  ]
}
```

### 2. Resolve-by-URL Endpoints

`GET /api/v1/keegate/entries/resolve/uuid/{uuid}`

`GET /api/v1/keegate/entries/resolve/query?...`

Purpose:

- resolve URL-friendly path shapes into normal entry-list responses
- support clients that resolve secrets directly from `kg://...` strings
- support both single-entry and multi-entry lookups through the same client URL mechanism

Authentication:

- required

Semantics:

- resolve endpoints always return the standard multi-entry response shape used for entry search
- clients should assume a resolve URL may return multiple entries
- when a client expects only one entry, it should use the first returned entry
- `GET /api/v1/keegate/entries/resolve/query?...` should be handled internally as `GET /api/v1/keegate/entries/query?...`
- if the UUID exists and is authorized, return a single-element `entries` array
- if the UUID does not exist, return `404 Not Found`
- if the UUID exists but is not authorized for the authenticated user, also return `404 Not Found`

These endpoints are the canonical translation targets for the KeeGate URL scheme described above.

### 3. Search Endpoint

`GET /api/v1/keegate/entries/query?...`

`POST /api/v1/keegate/entries/query`

Purpose:

- retrieve entries by flexible boolean search
- support nested `and` / `or` combinations across supported predicates
- support a URL-query form for simple client-driven lookups

Authentication:

- required

GET query-string form:

- supports simple flat filters using query parameters such as `title_contains`, `title_regex`, `tag`, `uuid`, and `limit`
- if multiple filter parameters are present, they should be combined with `and` semantics
- this is the form used by `kg:///query?...` resolution

Why keep `POST`:

- nested boolean expressions are awkward in query parameters
- JSON is easier to extend later
- regex patterns are less error-prone in JSON than in URL encoding

### Optional Convenience Search Endpoints

These are optional and can be added later if desired, but are not required for v1:

- `GET /api/v1/keegate/entries/search?title_contains=...`
- `GET /api/v1/keegate/entries/search?tag=...`

The core design should be built around:

- `GET /api/v1/keegate/entries/resolve/...` for single-string URL resolution
- `GET /api/v1/keegate/entries/query?...` for simple URL-query search
- `POST /api/v1/keegate/entries/query` for flexible structured search

## Query Language

### Request Shape

```json
{
  "filter": {
    "and": [
      { "tag": "prod" },
      {
        "or": [
          { "title_contains": "postgres" },
          { "title_regex": "(?i)db|database" },
          { "uuid": "2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e" }
        ]
      }
    ]
  },
  "options": {
    "limit": 100
  }
}
```

### Filter Grammar

Each filter node is exactly one of:

- `{ "title_contains": "<substring>" }`
- `{ "title_regex": "<regex>" }`
- `{ "tag": "<tag>" }`
- `{ "uuid": "<uuid>" }`
- `{ "and": [<filter>, <filter>, ...] }`
- `{ "or": [<filter>, <filter>, ...] }`

Rules:

- `and` and `or` arrays must contain at least one child filter
- nesting is allowed
- unknown filter keys return `400 Bad Request`
- invalid UUID syntax returns `400 Bad Request`
- invalid regex syntax returns `400 Bad Request`

### Predicate Semantics

#### `title_contains`

- compares against the entry title field
- performs a substring match
- case-insensitive by default in v1

Reasoning:

- case-insensitive substring matching is usually what password-manager callers expect
- regex remains available for callers that need exact control

#### `title_regex`

- compares against the entry title field
- uses Rust `regex`
- should be bounded by a maximum pattern length to avoid abuse

Notes:

- Rust `regex` does not support backreferences or look-around
- this limitation is acceptable for v1 and should be documented

#### `tag`

- matches entries containing the exact tag
- case-sensitive in v1

#### `uuid`

- matches one exact entry UUID

### Search Evaluation Order

For each request:

1. Authenticate the user.
2. Determine the user's allowed tag set.
3. Traverse all non-user entries in the database.
4. Evaluate the query filter against each candidate entry.
5. Apply authorization and keep only entries whose tags intersect with the user's tags.
6. Return the surviving entries.

Implementation note:

- for safety, authorization should be enforced even if the query itself already filters by tag

## Response Shape

### Search And Resolve Success Response

`200 OK`

```json
{
  "entries": [
    {
      "uuid": "2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e",
      "title": "Prod Postgres",
      "username": "db_admin",
      "password": "secret",
      "url": "https://db.example.com",
      "notes": "primary production database",
      "tags": ["prod", "database"],
      "group_path": ["Infrastructure", "Databases"]
    }
  ],
  "meta": {
    "count": 1,
    "limit": 100
  }
}
```

### Returned Fields

The v1 response should always include:

- `uuid`
- `title`
- `username`
- `password`
- `url`
- `notes`
- `tags`
- `group_path`

Field mapping from KeePass entry fields:

- `title` comes from the standard `Title` field
- `username` comes from the standard `UserName` field
- `password` comes from the standard `Password` field
- `url` comes from the standard `URL` field
- `notes` comes from the standard `Notes` field

Missing fields should be returned as `null`.

Field projection is not supported in v1. Clients always receive the full standard payload for each returned entry.

### Result Ordering

To keep behavior deterministic, sort results by:

1. title ascending
2. UUID ascending

### Result Limits

To protect the server:

- default `limit`: `100`
- maximum `limit`: `1000`

If the client requests a larger limit, clamp it to the maximum.

## Error Handling

### `401 Unauthorized`

Returned when:

- Basic Auth is missing
- Basic Auth is malformed
- the username does not exist in `KeeGate Users`
- the password does not match
- `KeeGate Users` does not exist
- duplicate user entries make identity ambiguous

Response:

- include `WWW-Authenticate: Basic realm="KeeGate API"`

### `404 Not Found`

Returned by `GET /api/v1/keegate/entries/resolve/uuid/{uuid}` when:

- the UUID does not exist
- the UUID exists but the authenticated user is not allowed to access it
- the UUID points to an entry in the reserved `KeeGate Users` group

Example body:

```json
{
  "error": "not_found",
  "message": "no accessible KeeGate entry matched the requested UUID"
}
```

### `400 Bad Request`

Returned when:

- the JSON body is malformed
- the filter tree is invalid
- a UUID is malformed
- a regex is invalid
- `limit` is invalid

Example body:

```json
{
  "error": "invalid_request",
  "message": "invalid regex in filter.title_regex"
}
```

### `500 Internal Server Error`

Returned when:

- the database cannot be loaded
- the query cannot be evaluated due to an internal error

Example body:

```json
{
  "error": "internal_error",
  "message": "failed to evaluate KeeGate query"
}
```

## Examples

### Resolve by absolute KeeGate URL

`kg://username:password@host/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`

translates to:

`https://username:password@host/api/v1/keegate/entries/resolve/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`

### Resolve by config-relative KeeGate URL

With:

```toml
[keegate]
url = "kg://username:password@host"
```

the value:

`kg:///uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`

translates to:

`https://username:password@host/api/v1/keegate/entries/resolve/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e`

### Resolve multiple entries by query URL

With:

```toml
[keegate]
url = "kg://username:password@host"
```

the value:

`kg:///query?tag=prod`

translates to:

`https://username:password@host/api/v1/keegate/entries/resolve/query?tag=prod`

The response should use the normal `entries` array shape. If a caller only needs one entry, it should use `entries[0]`.

### Find by UUID

```json
{
  "filter": {
    "uuid": "2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e"
  }
}
```

### Find by tag

```json
{
  "filter": {
    "tag": "prod"
  }
}
```

### Find by title substring

```json
{
  "filter": {
    "title_contains": "postgres"
  }
}
```

### Find by title regex

```json
{
  "filter": {
    "title_regex": "(?i)^prod.*postgres$"
  }
}
```

### Combine with OR

```json
{
  "filter": {
    "or": [
      { "tag": "prod" },
      { "tag": "shared" }
    ]
  }
}
```

### Combine with AND

```json
{
  "filter": {
    "and": [
      { "tag": "prod" },
      { "title_contains": "postgres" }
    ]
  }
}
```

### Nested AND / OR

```json
{
  "filter": {
    "and": [
      {
        "or": [
          { "tag": "prod" },
          { "tag": "staging" }
        ]
      },
      {
        "or": [
          { "title_contains": "postgres" },
          { "title_contains": "redis" }
        ]
      }
    ]
  }
}
```

## Internal Implementation Plan

### Routing

Add a new router subtree under `/api/v1/keegate` in the existing HTTP server.

Suggested handlers:

- `GET /api/v1/keegate/info`
- `GET /api/v1/keegate/entries/resolve/uuid/:uuid`
- `GET /api/v1/keegate/entries/resolve/query`
- `GET /api/v1/keegate/entries/query`
- `POST /api/v1/keegate/entries/query`

### Authentication Middleware

Create a dedicated KeeGate API auth layer. Do not share the existing WebDAV/sync/push auth middleware, request extension types, or config-backed credential checks.

In particular, the API auth layer must not:

- inspect the request path for a client ID
- depend on `config.toml` `clients`
- inject or consume the existing authenticated client request extension
- assume branch-oriented client identities

Instead, add separate API auth middleware that:

- reads Basic Auth credentials
- loads the database snapshot from the canonical store
- resolves the user from `KeeGate Users`
- injects an authenticated API user object into request extensions

Suggested request extension payload:

```rust
struct AuthedApiUser {
    username: String,
    tags: BTreeSet<String>,
}
```

This should remain a separate type from any existing HTTP auth identity struct so the two auth systems cannot be mixed accidentally.

### Query Engine

Add a small in-memory query evaluator over `StorageEntry`.

Suggested internal types:

```rust
enum EntryFilter {
    TitleContains(String),
    TitleRegex(regex::Regex),
    Tag(String),
    Uuid(uuid::Uuid),
    And(Vec<EntryFilter>),
    Or(Vec<EntryFilter>),
}
```

And an evaluation function along the lines of:

```rust
fn matches(entry: &StorageEntry, filter: &EntryFilter) -> bool
```

For `GET /api/v1/keegate/entries/resolve/uuid/:uuid`, the server can reuse the same entry indexing and authorization helpers while skipping the general boolean filter parser.

For `GET /api/v1/keegate/entries/query`, the server should parse the URL query string into the same internal filter representation used by `POST /api/v1/keegate/entries/query`, then execute the normal query path.

For `GET /api/v1/keegate/entries/resolve/query`, the server should reuse the same GET query handler as `GET /api/v1/keegate/entries/query`.

### Entry Traversal

Implement a traversal helper that walks the KeePass group tree and yields:

- a reference to each entry
- the entry's group path
- whether the entry belongs to the reserved `KeeGate Users` group

Suggested shape:

```rust
struct IndexedEntry<'a> {
    entry: &'a StorageEntry,
    group_path: Vec<String>,
    in_keegate_users_group: bool,
}
```

### Field Extraction

Add helper functions to pull standard KeePass fields from `StorageEntry.fields`:

- `Title`
- `UserName`
- `Password`
- `URL`
- `Notes`

### Logging

Log warnings for:

- missing `KeeGate Users` group on startup
- duplicate usernames in `KeeGate Users`
- malformed user entries

Do not log:

- supplied passwords
- matched secret values
- full query payloads if they might contain sensitive search patterns

## Security Notes

### Transport Security

The API returns passwords, so it should be documented as unsafe over plain HTTP on untrusted networks.

Recommendation:

- use HTTPS directly or place the server behind a TLS-terminating reverse proxy
- clients should treat `kg://username:password@host` values as secrets and redact them from logs, telemetry, and error messages

### Principle of Least Privilege

This design intentionally makes authorization data part of the KeePass database itself, which keeps administration simple and auditable.

Tradeoff:

- a user who can edit the database outside the API can also change API permissions by editing `KeeGate Users`

That is acceptable for v1 and consistent with KeePass being the source of truth.

### Regex Safety

Because regex comes from clients:

- cap regex length
- reject invalid patterns early
- consider request timeouts or per-request complexity guards if query load grows

Rust's regex engine avoids catastrophic backtracking, which is a good fit here.

## Open Decisions

These are the main choices worth confirming during implementation:

1. Whether `title_contains` should be case-insensitive, as proposed here, or exact-case.
2. Whether tags should remain case-sensitive, as proposed here.
3. How expressive the `/query?...` URL query-string grammar should be in v1 beyond the core supported search predicates.
4. Decided: field projection is not supported in v1; responses always return the full standard payload.

## Recommended v1 Scope

Implement only:

- `GET /api/v1/keegate/info`
- KeeGate URL parsing for `kg://username:password@host`, `kg://username:password@host/uuid/<uuid>`, `kg://username:password@host/query?...`, `kg:///uuid/<uuid>`, and `kg:///query?...`
- `GET /api/v1/keegate/entries/resolve/uuid/{uuid}`
- `GET /api/v1/keegate/entries/resolve/query?...`
- `GET /api/v1/keegate/entries/query?...`
- `POST /api/v1/keegate/entries/query`
- Basic Auth against `KeeGate Users`
- tag-intersection authorization
- title substring, title regex, tag, and UUID filters
- nested `and` / `or`
- deterministic sorting and bounded result limits

This keeps the first version small while fully satisfying the required retrieval and access-control behavior.
