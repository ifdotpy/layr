#!/bin/bash
# ci: macos
# `fetch` and `push` with `--line <name> --overlay <layer>` (docs/overlays.md section 1a): a fetch takes the history of the chosen lines,
# only on the chosen layers. The states of other lines stay on the remote and come on first read. Without `base` in
# `--overlay`, no state of the base comes.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
read id key <<<"$(env LAYR_ROOT="$T/y/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$id" --public-key "$key" >/dev/null
echo m0 > m0.txt; l add m0.txt >/dev/null; l save -qm m0
l line new side >/dev/null 2>&1
l push cloud -q >/dev/null 2>&1

yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
yl clone "file://$T/archive" p --heads --only-line main >/dev/null 2>&1
ok "the clone has main" test -f "$T/y/store/projects/p/lines/main/m0.txt"
has_state() { test -d "$T/y/store/projects/p/states/$1"; }

# New history on main and on side.
for i in 1 2; do echo m$i > m$i.txt; l add m$i.txt >/dev/null; l save -qm m$i; done
main_old=$(l log --json 2>/dev/null | python3 -c 'import json,sys; print([json.loads(l) for l in sys.stdin][1]["id"])')
cd "$LAYR_ROOT/projects/p/lines/side" || exit 1
for i in 1 2; do echo s$i > s$i.txt; l add s$i.txt >/dev/null; l save -qm s$i; done
side_head=$(l log --json 2>/dev/null | python3 -c 'import json,sys; print([json.loads(l) for l in sys.stdin][0]["id"])')
l push cloud -q >/dev/null 2>&1

cd "$T/y/store/projects/p/lines/main" || exit 1
out=$(yl fetch origin --overlay base --line nope 2>&1); code=$?
no "a line that is not in the store fails the fetch" test "$code" = 0
has "and names it" "line 'nope' is not in the store" "$out"

out=$(yl fetch origin --overlay nothere 2>&1); code=$?
no "an overlay that the project does not have fails the fetch" test "$code" = 0
has "and names it" "no overlay 'nothere'" "$out"
no "and takes no base state" has_state "$main_old"

out=$(yl fetch origin --line main 2>&1); code=$?
eq "a fetch of one line works" 0 "$code"
ok "it takes the history of main" has_state "$main_old"
no "it leaves the states of side on the remote" has_state "$side_head"
has "and says so" "restored on first read" "$out"

yl fetch origin >/dev/null 2>&1
ok "a plain fetch takes the states of side" has_state "$side_head"

# `push --line`: the states of that line go up; another line's new states stay here.
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
echo m3 > m3.txt; l add m3.txt >/dev/null; l save -qm m3
main_new=$(l log --json 2>/dev/null | python3 -c 'import json,sys; print([json.loads(l) for l in sys.stdin][0]["id"])')
cd "$LAYR_ROOT/projects/p/lines/side" || exit 1
echo s3 > s3.txt; l add s3.txt >/dev/null; l save -qm s3
side_new=$(l log --json 2>/dev/null | python3 -c 'import json,sys; print([json.loads(l) for l in sys.stdin][0]["id"])')
l push cloud --line main -q >/dev/null 2>&1
eq "a push of one line works" 0 "$?"
ok "it sends the new state of main" test -d "$T/archive/states/$main_new"
no "it keeps the new state of side here" test -d "$T/archive/states/$side_new"
l push cloud -q >/dev/null 2>&1
ok "a plain push sends it" test -d "$T/archive/states/$side_new"
no "--line needs fetch or push" l pull cloud --line main -q
no "--overlay and --no-caches do not go together" yl fetch origin --overlay base --no-caches
finish
