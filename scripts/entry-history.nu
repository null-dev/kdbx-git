#!/usr/bin/env nu

# Shows all commits that changed a specific KeePass entry, with diffs.
# Usage: nu scripts/entry-history.nu "example.com"
#        nu scripts/entry-history.nu "Work/Corp VPN" --store ./store.git
#        nu scripts/entry-history.nu "example.com" --branch laptop

def find-entry [group: record, parts: list<string>] {
    if ($parts | length) == 1 {
        let matches = $group.entries
            | where { |e| ($e.fields | get -o Title | default {} | get -o value | default "") == ($parts | first) }
        if ($matches | is-empty) { null } else { $matches | first }
    } else {
        let subs = $group.groups | where name == ($parts | first)
        if ($subs | is-empty) { null } else { find-entry ($subs | first) ($parts | skip 1) }
    }
}

def entry-to-text [entry: record] {
    ["Title" "UserName" "URL" "Notes" "Password" "Tags"] | each { |name|
        let val = ($entry.fields | get -o $name | default {} | get -o value | default "")
        $"($name): ($val)"
    } | str join "\n"
}

def entry-at-commit [git_dir: string, hash: string, entry_path: string] {
    let raw = try { ^git --git-dir $git_dir show $"($hash):db.json" } catch { return "" }
    let db = ($raw | from json)
    let entry = find-entry $db.root ($entry_path | split row "/")
    if $entry == null { return "" }
    entry-to-text $entry
}

def main [
    entry_path: string              # KeePass entry path, e.g. "example.com" or "Work/Corp VPN"
    --store: string = "."           # Path to the bare git store
    --branch: string = "main"       # Branch to inspect
] {
    let git_dir = ($store | path expand)

    let commits = (
        ^git --git-dir $git_dir log --format="%H|%aI|%s" $branch -- db.json
        | lines
        | where { |l| ($l | str length) > 0 }
        | each { |l|
            let p = $l | split row "|"
            {hash: ($p | get 0), date: ($p | get 1), subject: ($p | get 2)}
        }
    )

    if ($commits | is-empty) {
        print "No commits found in the store."
        return
    }

    mut found = false

    for i in 0..(($commits | length) - 1) {
        let c = $commits | get $i
        let curr = entry-at-commit $git_dir $c.hash $entry_path
        let prev = if ($i + 1) < ($commits | length) {
            entry-at-commit $git_dir ($commits | get ($i + 1)).hash $entry_path
        } else { "" }

        if $curr != $prev {
            $found = true
            print $"commit ($c.hash)"
            print $"Date:    ($c.date)"
            print $"         ($c.subject)"
            print ""

            let a = (mktemp)
            let b = (mktemp)
            $prev | save -f $a
            $curr | save -f $b
            let d = (^diff --color=always -u --label before --label after $a $b | complete)
            print $d.stdout
            rm $a $b
        }
    }

    if not $found {
        print $"No history found for entry: ($entry_path)"
    }
}
