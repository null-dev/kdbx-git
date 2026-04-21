# Web UI Plan

## Why Add A Web UI

`kdbx-git` already has a solid server core:

- WebDAV for full-database clients
- sync endpoints and SSE for `sync-local`
- push registration for mobile wakeups
- a read-only KeeGate HTTP API for entry lookup
- a git-backed history model that is easy to inspect

A web UI should build on that instead of replacing it.

The best first version is an operator-focused admin UI with a path to a second, more end-user-facing secret browser later.

## Product Goals

1. Make the server understandable at a glance.
2. Let an operator inspect sync health without needing git commands or manual API calls.
3. Expose the existing KeeGate read-only API through a friendly browser UI.
4. Keep security boundaries clear between:
   - server admin access
   - per-client WebDAV/sync credentials
   - KeeGate API users stored inside the database
5. Keep the frontend modern and pleasant to work in without letting it sprawl into an unbounded second product.

## Recommended Scope

### Phase 1: Admin UI First

Start with a read-only admin UI built in Svelte and backed by the Rust server's JSON APIs.

This gives the most value fastest:

- health/status dashboard
- configured client list
- branch and history visibility
- push registration visibility
- recent sync activity
- safe links and examples for WebDAV, sync-local, and KeeGate usage

### Phase 2: Secret Browser

Add an authenticated KeeGate browser UI for users who want to search/view entries in the browser without mounting WebDAV or scripting against the API.

This should be treated as a separate auth domain from the admin UI.

### Phase 3: Limited Admin Actions

After the read-only UI feels solid, add a small number of high-confidence write actions:

- create/regenerate a client password
- disable a client
- export a redacted support bundle
- unregister a stale push endpoint

Avoid database entry editing in the first web UI iterations.

## Recommended Architecture

### Frontend Approach

Use a dedicated Svelte app, ideally `SvelteKit`, with `shadcn-svelte` as the component foundation.

Recommended stack:

- `SvelteKit` for routing, layouts, SSR, and page data loading
- `shadcn-svelte` for accessible primitives and consistent UI building blocks
- Tailwind CSS, as expected by the `shadcn-svelte` ecosystem
- small live-update hooks for SSE-backed widgets where useful
- the existing Rust server continuing to own the real APIs and auth/session validation

Recommended repo shape:

- `server/` remains the API and core server
- add a new `web-ui/` app for the Svelte frontend
- in development, run the Svelte dev server separately
- in production, build static assets or an SSR bundle and serve it behind or through the Rust server

Why this fits your preference while still fitting the repo:

- Svelte is a good match for dashboards and forms without a lot of boilerplate
- `shadcn-svelte` gives the UI a strong component baseline without forcing a rigid design system
- the server already has the API boundaries we need
- the KeeGate browser will benefit from richer client-side interactions
- we can still keep the product intentionally small even if the frontend stack is more modern

The main tradeoff is extra frontend toolchain complexity compared with server-rendered HTML, but that seems acceptable given your stated preference.

### Rendering Strategy

Prefer SSR-first SvelteKit pages rather than a pure client-only SPA.

That means:

- route-level data loading for dashboard and detail pages
- server-rendered initial page responses
- client-side navigation after first load
- selective real-time enhancement where live state matters

This keeps the app fast and easier to secure while still feeling modern.

### Design Direction

Use `shadcn-svelte` components as primitives, not as the final product look.

The UI should feel intentional rather than default:

- clean, dense admin dashboards for server/operator pages
- stronger visual separation between admin pages and KeeGate secret-browser pages
- consistent tokens for status colors, branch/sync state, and warning severity
- careful use of drawers, dialogs, tables, badges, cards, and command-style search

## Integration Model

The project should use one integration model:

- SvelteKit handles UI routes under `/ui`
- Rust handles JSON and SSE routes under `/api/ui/v1/...` plus existing server APIs
- the frontend lives in its own dedicated workspace crate so its build pipeline is owned separately from the server crate

- develop `web-ui/` with the Svelte dev server
- build the frontend during release
- serve the built assets from the Rust server
- keep all API traffic same-origin in production

This keeps production deployment simpler:

- one primary server process
- no separate Node service in production
- same-origin cookies and API calls
- fewer moving pieces for self-hosting

## Route Layout

Suggested Svelte UI routes:

- `/ui/login`
- `/ui/`
- `/ui/clients`
- `/ui/clients/:id`
- `/ui/history`
- `/ui/push`
- `/ui/keegate`
- `/ui/settings`

Suggested backend JSON endpoints for the UI:

- `/api/ui/v1/status`
- `/api/ui/v1/clients`
- `/api/ui/v1/clients/:id`
- `/api/ui/v1/history`
- `/api/ui/v1/push`
- `/api/ui/v1/keegate/query`

Do not reuse the existing WebDAV auth middleware for UI routes.

If `SvelteKit` is used, the page routes would likely live as:

- `src/routes/ui/+layout.svelte`
- `src/routes/ui/login/+page.svelte`
- `src/routes/ui/+page.svelte`
- `src/routes/ui/clients/+page.svelte`
- `src/routes/ui/clients/[id]/+page.svelte`
- `src/routes/ui/history/+page.svelte`
- `src/routes/ui/push/+page.svelte`
- `src/routes/ui/keegate/+page.svelte`
- `src/routes/ui/settings/+page.svelte`

## Authentication Model

The UI needs its own auth story.

### Admin UI Auth

Add a new config section for one or more admin users, for example:

```toml
[web_ui]
enabled = true
listen_path = "/ui"

[[web_ui.admin_users]]
username = "admin"
password = "admin-password"
```

Use:

- secure HTTP-only cookies
- CSRF protection for any state-changing request
- a short idle timeout and explicit logout

Do not use:

- the KDBX master password as an admin login
- existing WebDAV client passwords as admin credentials
- KeeGate users as admin users

### KeeGate Browser Auth

For the end-user secret browser, reuse KeeGate credentials and authorization rules, but keep the session separate from admin auth.

That means:

- admin login sees server/operator pages
- KeeGate login sees only entries allowed by KeeGate tags
- no mixing of the two session types

## Information Architecture

### 1. Dashboard

Purpose: answer “is the server healthy?” in one view.

Show:

- server version/build info
- bind address
- git store path
- whether KeeGate API is enabled
- number of configured clients
- number of registered push endpoints
- latest `main` commit
- latest per-client branch tips
- whether `main` exists yet
- startup warnings such as missing `KeeGate Users`

Nice-to-have:

- live activity ticker using SSE
- quick badges for “healthy”, “warning”, “attention needed”
- card-based layout using `shadcn-svelte` card, badge, and table primitives

### 2. Clients Page

Purpose: inspect WebDAV/sync clients and their server-side state.

Show per client:

- client ID
- whether its branch exists
- current branch tip
- whether it is ahead/behind/diverged from `main`
- whether a push endpoint is registered
- last known push refresh time
- example WebDAV URL
- example `sync-local` config snippet

Potential future actions:

- rotate password
- disable client
- trigger a branch refresh from `main`

### 3. Client Detail Page

Purpose: drill into one client’s sync situation.

Show:

- auth/config summary, with secrets redacted by default
- branch history and recent commits
- last merge/promotion activity
- push endpoint metadata
- whether the client has outstanding divergence from `main`
- links to raw API endpoints for debugging

Stretch ideas:

- rendered commit graph for `main` vs client branch
- downloadable redacted diagnostic bundle
- side panel or modal flows for support actions

### 4. History Page

Purpose: make the git-backed nature of the project visible and useful.

Show:

- recent commits on `main`
- recent commits on each client branch
- commit author/subject/time
- changed entry counts if cheaply available
- per-commit detail view

Optional later:

- JSON diff summaries between commits
- branch comparison view
- “show entries changed in this revision”
- virtualized or paginated commit tables if the history gets large

### 5. Push Page

Purpose: inspect mobile instant-sync health.

Show:

- which clients have registered endpoints
- endpoint age / last refresh
- last delivery attempt
- last delivery result
- pruning/removal reasons for stale endpoints
- VAPID public key display/copy helper

This likely requires extending `sync-state.json` to store delivery metadata beyond the current subscription itself.

### 6. KeeGate Browser

Purpose: provide a user-facing way to search and view allowed entries.

Core features:

- login with KeeGate username/password
- search by title substring
- search by regex
- filter by tag
- lookup by UUID
- table of matching entries
- entry detail drawer/page
- copy username/password/url/notes
- reveal password on demand
- command-palette-style search UX for quick lookup

Important guardrails:

- never show entries outside KeeGate tag authorization
- do not include admin controls here
- default to masked passwords in lists
- log reveal/copy events if audit logging is added

### 7. Settings / Docs Page

Purpose: reduce setup friction.

Show:

- redacted effective config
- startup warnings
- copy-paste examples for:
  - WebDAV setup
  - `sync-local` config
  - KeeGate API usage
  - push registration flow
- links to README and API docs

## Features The UI Could Have

These are the strongest candidates, ordered roughly by value.

### High-Value MVP Features

- server health dashboard
- client list and detail views
- branch existence and tip visibility
- recent commit/history browser
- push endpoint visibility
- KeeGate API status/info display
- docs/examples page

### Strong Phase 2 Features

- KeeGate secret browser
- live updates via SSE
- branch divergence indicators
- per-client troubleshooting page
- branch comparison against `main`

### Good Stretch Features

- client password rotation
- temporary one-time enrollment links for new clients
- server-side audit log viewer
- entry change summaries derived from JSON diffs
- downloadable backups/export tools
- admin-triggered “resync this client from main”
- basic readonly metrics page

## Backend Work Needed

The UI can reuse a lot of existing logic, but it still needs new server-side APIs.

### Read-Only Server Status APIs

Add internal/admin JSON endpoints that expose:

- config summary with secrets redacted
- client list
- branch existence/tip IDs
- latest commit subjects and timestamps
- push registration state
- KeeGate availability/warnings

These should not scrape HTML pages; build them as proper typed handlers.

The Svelte app should consume these through typed client helpers so the frontend stays close to the Rust response models.

### Git/History Helpers

Likely new helper methods on `GitStore`:

- list branches
- get branch tip metadata
- get recent commit history
- compare branch tips
- maybe compute ahead/behind counts

### Push Metadata Expansion

Today the push state appears to store endpoints and VAPID keys.

For a useful UI, extend it to also track:

- created/updated timestamps
- last delivery attempt
- last delivery success
- last delivery failure message
- last HTTP response class or status

### Optional Activity Feed

For a live dashboard, add an admin SSE endpoint that emits events when:

- `main` advances
- a client branch advances
- a push endpoint is registered/removed
- a push delivery succeeds/fails

On the Svelte side, use this to progressively enhance the dashboard and client detail pages instead of making every screen fully real-time.

## Security Requirements

This part matters as much as the UI itself.

1. Keep auth domains separate.
2. Never expose stored config secrets back to the browser in plaintext.
3. Mask passwords by default and require explicit reveal.
4. Use POST-only actions plus CSRF protection for admin mutations.
5. Rate-limit login attempts.
6. Prefer secure cookies over long-lived bearer tokens for browser sessions.
7. Add click-to-copy carefully; avoid rendering sensitive values into logs or analytics.
8. Make it possible to disable the web UI entirely in config.
9. Assume the UI will eventually be exposed behind HTTPS only.

## Suggested Config Additions

```toml
[web_ui]
enabled = true
base_path = "/ui"
session_ttl_hours = 8

[[web_ui.admin_users]]
username = "admin"
password = "admin-password"
```

Possible later additions:

```toml
[web_ui]
allow_keegate_browser = true
audit_log_path = "./web-ui-audit.jsonl"
```

## Proposed Delivery Plan

### Milestone 1: Foundation

- add `web_ui` config section
- add admin session auth
- scaffold `web-ui/` with `SvelteKit`, Tailwind, and `shadcn-svelte`
- implement embedded frontend asset serving from the Rust server
- add app shell, top nav, login/logout, and shared layout
- add `/ui` dashboard shell

### Milestone 2: Read-Only Admin Dashboard

- status cards
- client list
- client detail page
- history page
- settings/docs page
- typed frontend API clients and shared status formatting utilities

At the end of this milestone, the UI is already useful even without any write actions.

### Milestone 3: Live State And Push Visibility

- add SSE-powered live refresh
- add push page
- extend sync-state metadata if needed
- add warning banners for stale/missing state

### Milestone 4: KeeGate Browser

- separate KeeGate login/session flow
- search form backed by existing KeeGate query logic
- result table and entry detail drawer/page
- masked secret reveal/copy UX
- polished `shadcn-svelte` search and detail components

### Milestone 5: Safe Admin Actions

- rotate client password
- remove stale push endpoint
- export redacted support bundle

Only add write actions that are easy to reason about and easy to audit.

## What I Would Build First

If we want the best value per unit of complexity within a Svelte-based UI, I would start with:

1. `/ui/login`
2. `/ui/` dashboard
3. `/ui/clients`
4. `/ui/clients/:id`
5. `/ui/history`
6. `/ui/settings`

That gives the server a real web face without forcing us to solve full browser-based secret management on day one.

Then I would add the KeeGate browser as a clearly separate second track.

## Summary Recommendation

Build the web UI in two layers:

- first, a SvelteKit admin UI for operators
- second, an optional KeeGate-powered secret browser for end users

Use `shadcn-svelte` as the component backbone, keep auth domains separate, and treat read-only observability as the MVP. That approach still fits the current axum + Rust architecture, reuses the existing server capabilities well, and gives the project a modern frontend without losing the phased, security-first rollout.
