#!/bin/bash
# ci: macos overlayfs
# `save --caches --save-back <line>` (docs/ci-trust.md): a CI job saves the caches it built on the save-back line of
# main, as a passing `checks run` does, and makes no source state. Another machine starts a line from main's head
# with that cache.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
read c_id c_key <<<"$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust archive "$c_id" --public-key "$c_key" >/dev/null
mkdir -p target; echo laptop > target/a; l save --caches -q
echo change > change.txt; l add change.txt >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
l push archive -q --caches >/dev/null 2>&1
head=$(l rev-parse main)

cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
cl clone "file://$T/archive" p --caches-of main >/dev/null 2>&1
cd "$T/c/store/projects/p/lines/main" || exit 1
cl line new job main >/dev/null 2>&1
cd "$T/c/store/projects/p/lines/job" || exit 1
echo job > target/a
# A source change that the job built: the saved cache must name these bytes, not the laptop's.
echo job-edit > change.txt
states=$(cl log --all --json 2>/dev/null | wc -l)
no "--save-back needs --caches" cl save --save-back main
no "and a known line" cl save --caches --save-back nosuch
out=$(cl save --caches --save-back main --json)
plat=$(echo "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["line"])')
has "the caches go to the save-back line of main" "main.checks-" "$plat"
main_id=$(cd "$T/c/store/projects/p/lines/main" && cl log --json 2>/dev/null | head -1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
has "with main's head as their source" "\"source\":\"$main_id\"" "$out"
has "and the cache folder" '"path":"target"' "$out"
ok "the saved cache records the source bytes of the job" grep -q " $(git hash-object change.txt) change.txt\$" target/.layr-times
eq "no source state is made" "$states" "$(cl log --all --json 2>/dev/null | wc -l)"
cl push origin -q --caches >/dev/null 2>&1

cd "$P/main" || exit 1
l fetch archive -q >/dev/null 2>&1
has "the laptop has the save-back line" "$plat" "$(l line list 2>&1)"
l line new w "$head" >/dev/null 2>&1
eq "a new line from main's head takes the cache that the job built" job "$(cat "$P/w/target/a" 2>&1)"
finish
