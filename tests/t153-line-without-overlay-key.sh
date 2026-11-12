#!/bin/bash
# ci: macos
# A machine that is trusted but has no key of a cache overlay yet (docs/cache-overlays.md): `remote trust`
# writes its store slot at once, and the overlay's reader slot comes with the next cache save or push of a
# key manager. Until then its new lines start that folder cold, with a message; they do not fail.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
mkdir -p target; echo laptop > target/a; l save --overlays -q; l push cloud -q >/dev/null 2>&1
r=$(env LAYR_ROOT="$T/r/store" "$LAYR" remote self --json)
rid=$(echo "$r" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
rkey=$(echo "$r" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote trust cloud "$rid" --public-key "$rkey" >/dev/null
rl() { env LAYR_ROOT="$T/r/store" "$LAYR" "$@"; }
rl clone "file://$T/archive" p >/dev/null 2>&1
cd "$T/r/store/projects/p/lines/main" || exit 1
out=$(rl line new runner main 2>&1); code=$?
eq "a new line works without the overlay's key" 0 "$code"
has "it says why the folder starts cold" "no cache for target" "$out"
ok "the line is there" test -d "$T/r/store/projects/p/lines/runner"
# After the next cache save and push of the laptop (a key manager) the runner reads the caches.
(cd "$LAYR_ROOT/projects/p/lines/main" && l save --overlays -q && l push cloud -q >/dev/null 2>&1)
rl fetch origin -q >/dev/null 2>&1
rl line new warm main >/dev/null 2>&1
eq "after the next push and fetch it starts warm" laptop "$(cat "$T/r/store/projects/p/lines/warm/target/a" 2>&1)"
finish
