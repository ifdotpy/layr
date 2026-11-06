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

# A push that stopped before its tree leaves parts that no tree names. After 24 hours (writers stop at +23 h) the
# sweep of a push deletes them; a younger one stays, as it may belong to a push in flight.
dir=$(dirname "$(find "$T/archive/overlays" -name '*.tree.age' | head -1)")/..
mkdir -p "$dir/01a1aaaa-0000-7000-8000-000000000001" "$dir/01a1aaaa-0000-7000-8000-000000000002"
echo old > "$dir/01a1aaaa-0000-7000-8000-000000000001/deadbeef.0000.data.age"
touch -t 202101010000.00 "$dir/01a1aaaa-0000-7000-8000-000000000001/deadbeef.0000.data.age"
echo young > "$dir/01a1aaaa-0000-7000-8000-000000000002/deadbeef.0000.data.age"
head -c 20000 /dev/urandom > target/big3; l save --caches -q; l push archive -q --caches >/dev/null 2>&1
no "the sweep deletes an old part that no tree names" test -e "$dir/01a1aaaa-0000-7000-8000-000000000001/deadbeef.0000.data.age"
ok "and keeps a young one" test -e "$dir/01a1aaaa-0000-7000-8000-000000000002/deadbeef.0000.data.age"
rm -rf "$dir/01a1aaaa-0000-7000-8000-000000000001" "$dir/01a1aaaa-0000-7000-8000-000000000002"

# fsck --caches --remote reports dead cache states that a push has not deleted yet; the daemon's
# pass deletes them in a remote that nobody pushes to.
for i in 4 5; do head -c 20000 /dev/urandom > target/big$i; l save --caches -q; done
l push archive -q --caches >/dev/null 2>&1
echo changed > target/big5; l save --caches -q
l line new y main >/dev/null 2>&1
(cd "$P/y" && head -c 20000 /dev/urandom > target/yonly && l save --caches -q && l push archive -q --caches >/dev/null 2>&1)
l line delete -D y >/dev/null
out=$(l fsck --caches --remote archive 2>&1); rc=$?
eq "fsck --caches --remote exits 1 with dead states" 1 "$rc"
has "it names them" "1 unreachable cache state(s)" "$out"
"$LAYR" daemon --tick 1 --socket "$T/d.sock" > "$T/daemon.log" 2>&1 &
DPID=$!
for _ in $(seq 1 100); do grep -q "deleted" "$T/daemon.log" 2>/dev/null && break; sleep 0.2; done
kill $DPID 2>/dev/null; wait $DPID 2>/dev/null
has "the daemon deletes them" "deleted 1 dead cache state(s)" "$(cat "$T/daemon.log")"
has "then nothing is unreachable" "unreachable cache data: 0 object(s), 0 bytes" "$(l fsck --caches --remote archive 2>&1)"

# A reader removal starts a new key epoch. Its push writes the caches there; nothing reads the old
# epoch now, so the push deletes the cache states in it.
ov=$(l overlay list 2>/dev/null | awk '{print $1}' | grep -m1 '^cache')
r1=machine:$(uuidgen | tr 'A-Z' 'a-z')
r2=machine:$(uuidgen | tr 'A-Z' 'a-z')
l overlay readers "$ov" "$r1" "$r2" >/dev/null 2>&1
l push archive -q --caches >/dev/null 2>&1
epochs() { find "$T/archive/overlays" -path '*/epochs/*' -name '*.tree.age' | sed 's|.*/epochs/\([^/]*\)/.*|\1|' | sort -u | wc -l | tr -d ' '; }
eq "one epoch has cache states before the key change" 1 "$(epochs)"
l overlay readers "$ov" "$r2" >/dev/null 2>&1
out=$(l push archive -q --caches 2>&1)
has "the push names the deleted states of the old epoch" "of older key epochs" "$out"
eq "only the new epoch has cache states" 1 "$(epochs)"
eq "and it has the kept states" 2 "$(trees)"
has "nothing is unreachable" "unreachable cache data: 0 object(s)" "$(l fsck --caches --remote archive 2>&1)"

mkdir -p "$T/b"
env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/b/store/projects/p/lines
(cd "$Q/main" && env LAYR_ROOT="$T/b/store" "$LAYR" line new far main >/dev/null 2>&1)
ok "a clone still starts warm" cmp -s "$P/main/target/big4" "$Q/far/target/big4"
finish
