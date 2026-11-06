#!/bin/bash
# ci: macos overlayfs
# A line of another machine that this machine never made a folder for (a CI line that a fetch brought) can be
# deleted here: the record alone deletes it, as there is no folder to keep or trash.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
read b_id b_key <<<"$(env LAYR_ROOT="$T/b/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$b_id" --public-key "$b_key" >/dev/null
l push cloud -q >/dev/null 2>&1

bl() { env LAYR_ROOT="$T/b/store" "$LAYR" "$@"; }
(cd "$T" && bl clone "file://$T/archive" p --heads >/dev/null 2>&1)
(cd "$T/b/store/projects/p/lines/main" && bl line new ci-linux-1 main >/dev/null 2>&1 && cd ../ci-linux-1 && echo ci > ci.txt && bl add ci.txt >/dev/null && bl save -qm ci && bl push origin -q >/dev/null 2>&1)

l fetch cloud -q >/dev/null 2>&1
has "the fetched line is known here" ci-linux-1 "$(l line list 2>&1)"
no "and has no folder here" test -d "$LAYR_ROOT/projects/p/lines/ci-linux-1"
out=$(l line delete -D ci-linux-1 2>&1)
has "it can be deleted here" "Deleted line ci-linux-1" "$out"
no "it is gone from the list" grep -q ci-linux-1 <<<"$(l line list 2>&1)"
l push cloud -q >/dev/null 2>&1
# A machine that clones later sees the delete (the machine that made the line keeps its own line).
read c_id c_key <<<"$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$c_id" --public-key "$c_key" >/dev/null
l push cloud -q >/dev/null 2>&1
(cd "$T" && env LAYR_ROOT="$T/c/store" "$LAYR" clone "file://$T/archive" p --heads >/dev/null 2>&1)
no "another machine sees the delete" grep -q ci-linux-1 <<<"$(cd "$T/c/store/projects/p/lines/main" && env LAYR_ROOT="$T/c/store" "$LAYR" line list 2>&1)"
garbage_check
finish
