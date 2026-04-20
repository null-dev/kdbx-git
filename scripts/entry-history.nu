#!/usr/bin/env nu

# Shows all commits that changed a specific KeePass entry, with diffs.
# Usage: nu scripts/entry-history.nu <uuid>
#        nu scripts/entry-history.nu <uuid> --branch laptop

def collect-entries [group: record] {
    $group.entries ++ ($group.groups | each { |g| collect-entries $g } | flatten)
}

def find-entry [root: record, uuid: string] {
    let matches = collect-entries $root | where uuid == $uuid
    if ($matches | is-empty) { null } else { $matches | first }
}

def entry-at-commit [git_dir: string, hash: string, uuid: string] {
    let result = (^git --git-dir $git_dir show $"($hash):db.json" | complete)
    if $result.exit_code != 0 { return "" }
    let entry = find-entry ($result.stdout | from json).root $uuid
    if $entry == null { return "" }
    $entry | to json --indent 2
}

def main [
    uuid: string                    # UUID of the entry to inspect
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

    print $"Scanning ($commits | length) commits..."

    # Phase 1: fetch all entry texts in parallel, preserving commit order.
    let commits_asc = ($commits | reverse)
    let texts = ($commits_asc | par-each --keep-order { |c|
        entry-at-commit $git_dir $c.hash $uuid
    })

    # Phase 2: sequential pass to diff adjacent results
    mut prev_text = ""
    mut changes = []
    for i in 0..(($commits_asc | length) - 1) {
        let curr_text = $texts | get $i
        if $curr_text != $prev_text {
            $changes = ($changes | append {commit: ($commits_asc | get $i), before: $prev_text, after: $curr_text})
        }
        $prev_text = $curr_text
    }

    if ($changes | is-empty) {
        print $"No history found for entry: ($uuid)"
        return
    }

    let separator = (1..72 | each { "─" } | str join)

    # Display newest-first to match git log convention
    for change in ($changes | reverse) {
        let c = $change.commit
        print $separator
        print $"commit ($c.hash)"
        print $"Date:    ($c.date)"
        print $"         ($c.subject)"
        print ""

        let a = (mktemp)
        let b = (mktemp)
        $change.before | save -f $a
        $change.after  | save -f $b
        let d = (^diff --color=always -u --label before --label after $a $b | complete)
        print $d.stdout
        rm $a $b
    }
    print $separator
}
