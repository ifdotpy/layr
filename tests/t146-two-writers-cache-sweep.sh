#!/bin/bash
# ci: macos overlayfs
# Two machines push caches to one store remote (docs/cache-overlays.md, section 7). The sweep of
# one machine never deletes a cache state that only the other knows yet, keeps the states of the
# other's live lines, and the data of a deleted line goes with the push of its writer. After a
# fetch and a push, no unreachable cache data is left.
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
head -c 20000 /dev/urandom > target/a1; l save --caches -q

# Machine c: trusted, so it reads the cache overlay through its own slot and signs its pushes.
c=$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json)
c_id=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
c_key=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote trust archive "$c_id" --public-key "$c_key" >/dev/null
l save --caches -q
l push archive -q --caches >/dev/null 2>&1
env LAYR_ROOT="$T/c/store" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/c/store/projects/p/lines
cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
remote=origin

states() { find "$T/archive/overlays" -name '*.tree.age' 2>/dev/null | sed 's|.*/states/\([^/]*\)/.*|\1|' | sort -u; }
before=$(states)
(cd "$Q/main" && cl line new cx main >/dev/null 2>&1)
(cd "$Q/cx" && head -c 20000 /dev/urandom > target/conly && cl save --caches -q && cl push "$remote" -q --caches >/dev/null 2>&1)
cstate=$(comm -13 <(echo "$before") <(states) | head -1)
ok "machine c pushes a cache state of its line" test -n "$cstate"

# Machine a has not fetched: the state is not known there, and a's sweep keeps it.
for i in 2 3; do head -c 20000 /dev/urandom > target/a$i; l save --caches -q; l push archive -q --caches >/dev/null 2>&1; done
has "a's push keeps the state that only c knows" "$cstate" "$(states)"
has "fsck counts it as kept, not unreachable" "1 state(s) not known here kept" "$(l fsck --caches --remote archive 2>&1)"

# After a fetch a knows it, and the line of c is live: it stays.
l fetch archive -q >/dev/null 2>&1
l push archive -q --caches >/dev/null 2>&1
has "after a fetch the state of c's live line stays" "$cstate" "$(states)"
has "and nothing is unreachable for a" "unreachable cache data: 0 object(s)" "$(l fsck --caches --remote archive 2>&1)"

# c deletes its line: its own push deletes the line's cache data.
(cd "$Q/main" && cl line delete -D cx >/dev/null 2>&1 && cl push "$remote" -q --caches >/dev/null 2>&1)
no "c's push deletes the cache state of its deleted line" grep -q "$cstate" <<<"$(states)"
has "nothing is unreachable for c" "unreachable cache data: 0 object(s)" "$(cd "$Q/main" && cl fsck --caches --remote "$remote" 2>&1)"
# c had not fetched, so its pass kept the old main state that its line started from. a knows that
# main moved on: after a fetch it reports that state, and its next push deletes it.
l fetch archive -q >/dev/null 2>&1
has "a reports the old base state of c's line" "1 unreachable cache state(s)" "$(l fsck --caches --remote archive 2>&1)"
l push archive -q --caches >/dev/null 2>&1
has "a's push deletes it" "unreachable cache data: 0 object(s)" "$(l fsck --caches --remote archive 2>&1)"
eq "the remote keeps main's newest cache state and the empty first state" 2 "$(states | wc -l | tr -d ' ')"
l line new warm main >/dev/null 2>&1
ok "a new line of a still starts warm" cmp -s "$P/main/target/a3" "$P/warm/target/a3"
finish
