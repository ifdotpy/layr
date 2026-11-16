#!/bin/bash
# Issue #443: on btrfs a save writes its objects incrementally from the native copy of the line head (a btrfs send
# stream between the two snapshots). The free step after a save keeps that copy, so the next save after a small
# change is incremental too, and its tree equals a full write of the same snapshot.
# Btrfs only, as root (the send stream needs it): macOS and OverlayFS write their objects on other paths.
if [ "$(uname -s)" != Linux ] || [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ] || [ "$(id -u)" != 0 ]; then
    echo "$(basename "$0") (btrfs as root only): 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"

DB() { # DB <sql>: one query on the database of project p
    python3 -I -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); [print("|".join("" if v is None else str(v) for v in r)) for r in c.execute(sys.argv[2])]' \
        "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "$1"
}
json_field() { # json_field <field> <json>
    python3 -I -c 'import json,sys; print(json.loads(sys.argv[2])[sys.argv[1]])' "$1" "$2"
}
head_of_main() { DB "SELECT head FROM lines WHERE name='main'"; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l migrate >/dev/null 2>&1
eq "the project keeps its states as objects" "true" "$(l config objects.enabled 2>/dev/null | tr -d '"' | tail -1)"

echo a >> notes.txt
ok "a first named save" l save -qam "first"
first=$(head_of_main)
ok "the line head keeps its native copy after the command" test -d "$P/states/$first"

# A small change: one file changes, one is new, one gets a new mode.
echo b >> notes.txt
echo new > src/new.rs
chmod 755 src/main.rs
l add src/new.rs >/dev/null
out=$(LAYR_TRACE=1 l save -qam "second" 2>&1)
eq "the second save succeeds" 0 "$?"
second=$(head_of_main)
has "the second save writes its objects incrementally" "objects: incremental write" "$out"
case "$out" in *"objects: full write"*) FAIL=$((FAIL + 1)); echo "FAIL and no full write";; *) PASS=$((PASS + 1)); echo "PASS and no full write";; esac
ok "the new head keeps its native copy" test -d "$P/states/$second"
no "the replaced head gives up its native copy" test -d "$P/states/$first"

# A full write of the native snapshot gives the same tree as the incremental write.
recorded=$(DB "SELECT json_extract(rec,'\$.tree') FROM states WHERE id='$second'")
v=$(l objects verify "$second" --cold --json 2>/dev/null)
eq "the incremental tree equals a full write" "$recorded" "$(json_field tree "$v" 2>/dev/null)"
eq "a checkout of it equals the snapshot" "[]" "$(json_field differences "$v" 2>/dev/null)"
finish
