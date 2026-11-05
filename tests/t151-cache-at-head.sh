#!/bin/bash
# ci: macos
# `line new X <line>` takes the cache saved with the line's head (docs/cache-overlays.md): when the
# line's own cache is of an older head and another machine (CI) saved a cache with the head itself,
# the new line gets that one. A cache of the line at its head still comes first.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
mkdir -p target; echo laptop-old > target/a; l save --caches -q
echo change > change.txt; l add change.txt >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
# The CI machine is trusted after the caches were saved: the push alone gives it the caches.
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
c=$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json)
c_id=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
c_key=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote trust archive "$c_id" --public-key "$c_key" >/dev/null
l push archive -q --caches >/dev/null 2>&1
cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
cl clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/c/store/projects/p/lines
(cd "$Q/main" && cl line new ci main >/dev/null 2>&1)
eq "the CI line starts from the laptop's older cache" laptop-old "$(cat "$Q/ci/target/a" 2>&1)"
(cd "$Q/ci" && echo ci > target/a && cl save --caches -q && cl push origin -q --caches >/dev/null 2>&1)
l fetch archive -q >/dev/null 2>&1
l line new w main >/dev/null 2>&1
eq "a new line of main takes the cache that CI saved with main's head" ci "$(cat "$P/w/target/a" 2>&1)"
echo laptop-new > target/a; l save --caches -q
l line new w2 main >/dev/null 2>&1
eq "a cache of the line at its head comes first" laptop-new "$(cat "$P/w2/target/a" 2>&1)"
finish
