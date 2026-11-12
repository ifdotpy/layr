#!/bin/bash
# ci: macos
# `line new X <line>` takes the shared cache saved with the line's head (docs/cache-overlays.md): when the
# line's own cache is of an older head and a protected line or a check runner saved a cache with the head
# itself, the new line gets that one. A user's cache of an unprotected line at that head does not count, and
# a cache of the line at its head still comes first. A machine trusted after the caches were saved gets them
# at the next push.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
mkdir -p target; echo laptop-old > target/a; l save --overlays -q
echo change > change.txt; l add change.txt >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
# The CI machine is trusted after the caches were saved: the push alone gives it the caches.
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
c=$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json)
c_id=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
c_key=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote trust archive "$c_id" --public-key "$c_key" >/dev/null
l push archive -q >/dev/null 2>&1
cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
cl clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/c/store/projects/p/lines
(cd "$Q/main" && cl line new ci main >/dev/null 2>&1)
eq "the CI line starts from the laptop's older cache" laptop-old "$(cat "$Q/ci/target/a" 2>&1)"
(cd "$Q/ci" && echo ci > target/a && cl save --overlays -q && cl push origin -q >/dev/null 2>&1)
l fetch archive -q >/dev/null 2>&1
l line new w main >/dev/null 2>&1
eq "a cache of an unprotected line at main's head is not taken" laptop-old "$(cat "$P/w/target/a" 2>&1)"
# A protected line at main's head saves a shared cache with that head.
l line new rel main >/dev/null 2>&1
l protect rel >/dev/null 2>&1
(cd "$P/rel" && echo shared > target/a && l save --overlays -q)
l line new w3 main >/dev/null 2>&1
eq "a new line of main takes the shared cache saved with main's head" shared "$(cat "$P/w3/target/a" 2>&1)"
echo laptop-new > target/a; l save --overlays -q
l line new w2 main >/dev/null 2>&1
eq "a cache of the line at its head comes first" laptop-new "$(cat "$P/w2/target/a" 2>&1)"
finish
