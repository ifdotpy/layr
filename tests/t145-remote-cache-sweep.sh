#!/bin/bash
# ci: macos overlayfs
# A push deletes the dead cache states of a cache overlay in the store remote (docs/cache-overlays.md):
# the remote keeps the trees that the records keep (the newest `history` states of each live line on
# each platform) and the data parts that they link. A clone still starts warm.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
mkdir -p target
trees() { find "$T/archive/overlays" -name '*.tree.age' 2>/dev/null | wc -l | tr -d ' '; }
parts() { find "$T/archive/overlays" -name '*.data.age' 2>/dev/null | wc -l | tr -d ' '; }
# Each save makes a new file, so each state has a part of its own.
for i in 1 2 3; do head -c 20000 /dev/urandom > target/big$i; l save --caches -q; l push archive -q --caches >/dev/null 2>&1; done
eq "the remote keeps the newest cache state of main and the empty first state" 2 "$(trees)"
first_parts=$(parts)

l line new x main >/dev/null 2>&1
cd "$P/x" || exit 1
head -c 20000 /dev/urandom > target/xonly; l save --caches -q
l push archive -q --caches >/dev/null 2>&1
eq "a line's cache state is in the remote" 3 "$(trees)"
cd "$P/main" || exit 1
l line delete -D x >/dev/null
l push archive -q --caches >/dev/null 2>&1
eq "after the line delete and a push its cache state is gone from the remote" 2 "$(trees)"
eq "and its data part too" "$first_parts" "$(parts)"

mkdir -p "$T/b"
env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/b/store/projects/p/lines
(cd "$Q/main" && env LAYR_ROOT="$T/b/store" "$LAYR" line new far main >/dev/null 2>&1)
ok "a clone still starts warm" cmp -s "$P/main/target/big3" "$Q/far/target/big3"
finish
