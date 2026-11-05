#!/bin/bash
# ci: macos
# A check runner gives warmth back (docs/checks.md): runner c checks the head of protected main and pushes, and the
# laptop a starts a new line from that head with the cache that c's check built. One build per head.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
c=$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json)
c_id=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
c_key=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote trust archive "$c_id" --public-key "$c_key" >/dev/null
mkdir -p target; echo laptop > target/a; l save --caches -q
echo change > change.txt; l add change.txt >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
l config check.unit 'test "$(cat target/a)" = laptop && echo runner > target/a' >/dev/null
l config checks.lines '{"main":["unit"]}' >/dev/null
l push archive -q --caches >/dev/null 2>&1
head=$(l rev-parse main)

cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
cl clone "file://$T/archive" p >/dev/null 2>&1
cd "$T/c/store/projects/p/lines/main" || exit 1
cl config --local checks.runner true >/dev/null
out=$(cl checks run --state main --json 2>/dev/null)
has "the runner checks main's head warm" '"exit":0' "$out"
cl push origin -q --caches >/dev/null 2>&1

cd "$P/main" || exit 1
l fetch archive -q >/dev/null 2>&1
eq "the laptop has the runner's result" 0 "$(l checks pending --state main --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')"
l line new w "$head" >/dev/null 2>&1
eq "a new line of the laptop from that head takes the cache that the check built" runner "$(cat "$P/w/target/a" 2>&1)"
finish
