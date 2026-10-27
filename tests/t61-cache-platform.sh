#!/bin/bash
# The platform key of caches (docs/cache-overlays.md, section 6): a line takes a cache only when
# the key files hash the same; a layr remote carries cache overlays to another store, where new
# lines start warm. One user, so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
echo v1 > $T/src/Cargo.lock
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
l config cache.key-files '["Cargo.lock", "rust-toolchain.toml"]'
rm -rf target; mkdir target; echo built-v1 > target/out
l save --caches -q
has "the cache overlay has the key files" "Cargo.lock" "$(l config overlay.cache.target.key-files)"

# 1. Same key files: warm. Changed key files: cold, until a new save --caches.
l line new same main >/dev/null
eq "same lock file: the line starts warm" built-v1 "$(cat $P/same/target/out 2>/dev/null)"
echo v2 > Cargo.lock
l save -a -m "update the lock file" -q
l line new changed main >/dev/null
no "another lock file: the line starts cold" test -e $P/changed/target/out
echo built-v2 > target/out
l save --caches -q
l line new again main >/dev/null
eq "after a save with the new lock file it is warm again" built-v2 "$(cat $P/again/target/out 2>/dev/null)"
ok "the key files can be cleared" l overlay set cache.target --key-files none
no "a bad key file path fails" l overlay set cache.target --key-files ../x

# 2. A store carries the cache overlay; a clone on the same platform starts warm.
l overlay set cache.target --key-files Cargo.lock >/dev/null
l save --caches -q
l remote add archive "file://$T/archive" --paper-key-out $T/paper.txt >/dev/null 2>&1
ok "push to the store" l push archive -q
new_fs $T/b 2G
out=$(env LAYR_ROOT=$T/b/store LAYR_PAPER_KEY_FILE=$T/paper.txt $LAYR clone "file://$T/archive" p 2>&1)
has "the clone takes the cache overlay" "overlay cache.target" "$out"
cd $T/b/store/projects/p/lines/main
env LAYR_ROOT=$T/b/store $LAYR line new warm main >/dev/null 2>&1
eq "a new line in the clone starts warm" built-v2 "$(cat $T/b/store/projects/p/lines/warm/target/out 2>/dev/null)"

# 3. A machine the project trusts reads the cache overlay through its own machine slot.
new_fs $T/c 2G
c=$(env LAYR_ROOT=$T/c/store $LAYR remote self --json)
c_id=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
c_key=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
cd $P/main
ok "trust machine c" l remote trust archive $c_id --public-key $c_key
l save --caches -q
has "machine c reads the cache overlay" "machine:$c_id" "$(l overlay readers cache.target)"
ok "push again" l push archive -q
out=$(env LAYR_ROOT=$T/c/store $LAYR clone "file://$T/archive" p 2>&1)
has "machine c clones the cache overlay without the paper key" "overlay cache.target" "$out"

finish
