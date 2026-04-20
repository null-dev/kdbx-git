#!/usr/bin/env nu

# Initializes an example kdbx-git server in the current working directory for testing.
# Usage: nu scripts/example.nu

def main [] {
    let workspace = ($env.CURRENT_FILE | path dirname | path dirname)
    let db_pw = "correct horse battery staple"
    let db = "seed.kdbx"
    let config = "config.toml"

    # Create KeePass database
    print "Creating sample KeePass database..."
    $"($db_pw)\n($db_pw)\n" | ^keepassxc-cli db-create -p $db

    # Add top-level entries
    print "Adding sample entries..."
    $"($db_pw)\n" | ^keepassxc-cli add -q -u "alice" --url "https://example.com" -g $db "example.com"
    $"($db_pw)\n" | ^keepassxc-cli add -q -u "bob@github.com" --url "https://github.com" -g $db "github.com"

    # Create Work group and add entries to it
    print "Creating Work group..."
    $"($db_pw)\n" | ^keepassxc-cli mkdir -q $db "Work"
    $"($db_pw)\n" | ^keepassxc-cli add -q -u "alice" --url "https://vpn.corp.example.com" -g $db "Work/Corp VPN"
    $"($db_pw)\n" | ^keepassxc-cli add -q -u "alice@corp.example.com" --url "https://mail.corp.example.com" -g $db "Work/Corporate Mail"

    # Write server config
    print "Writing config.toml..."
    $"git_store = \"./store.git\"
bind_addr = \"0.0.0.0:8080\"

[database]
password = \"($db_pw)\"

[[clients]]
id = \"laptop\"
password = \"laptop-password\"

[[clients]]
id = \"phone\"
password = \"phone-password\"
" | save $config

    # Import into git store
    print "Importing database into git store..."
    cargo run --manifest-path $"($workspace)/Cargo.toml" -p kdbx-git -- init --config $config $db

    # Remove seed database
    print "Cleaning up seed database..."
    rm $db

    # Start server
    print "Starting kdbx-git server on 0.0.0.0:8080 (Ctrl+C to stop)..."
    cargo run --manifest-path $"($workspace)/Cargo.toml" -p kdbx-git -- --config $config
}
