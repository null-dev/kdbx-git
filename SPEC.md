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
- When a client reads their database file:
  - A database file is dynamically constructed from the contents of the client's branch and returned to the client
- When a client writes their database file:
  - The new database file is decrypted and it's contents written to the client's branch as a new commit.
  - The server then attempts to create a new commit on the "main" branch that merges the client's branch into the "main" branch.
  - If the server was successfully able to perform the merge, it then attempts to merge the main branch into all the client branches

### Other dependencies
- Use eyre + color_eyre for error handling
- Use axum for serving HTTP
- WebDAV handling: https://github.com/messense/dav-server-rs
- For logging/observability, use: tracing, tracing-subscriber (with env-filter)

### Other notes
- It should be possible to inspect the storage using the regular git CLI