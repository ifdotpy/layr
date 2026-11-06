#!/bin/bash
# ci: macos overlayfs
# `checks.changes` plans the newest version of each open change only: an older version is superseded, and a change
# that a protected line holds or whose line is deleted is done. Newer states come first.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l config check.smoke 'true' >/dev/null
l config checks.changes '["smoke"]' >/dev/null
pending() { l checks pending --json | python3 -c 'import json,sys; print(" ".join(sorted({p["state"] for p in json.load(sys.stdin)["pending"]})))'; }
ids() { l log "$1" -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }

l line new a main -q && (cd "$P/a" && echo a1 > a.txt && l add a.txt >/dev/null && l save -qm a)
a1=$(ids a)
(cd "$P/a" && echo a2 > a.txt && l save -qam a --amend)
a2=$(ids a)
l line new b main -q && (cd "$P/b" && echo b > b.txt && l add b.txt >/dev/null && l save -qm b)
b=$(ids b)
l line new c main -q && (cd "$P/c" && echo c > c.txt && l add c.txt >/dev/null && l save -qm c)
out=$(pending)
has "the newest version of an open change is planned" "$a2" "$out"
no "an older version is not" grep -q "$a1" <<<"$out"
has "another open change is planned" "$b" "$out"

# b goes into protected main: done.
l merge b -q >/dev/null 2>&1 || l merge b >/dev/null
l protect main >/dev/null 2>&1 || true
no "a change that a protected line holds is not planned" grep -q "$b" <<<"$(pending)"
# c's line goes: done.
c=$(ids c)
l line delete -D c >/dev/null
no "a change whose line is deleted is not planned" grep -q "$c" <<<"$(pending)"
has "the open change is still planned" "$a2" "$(pending)"
finish
