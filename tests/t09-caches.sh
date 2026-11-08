#!/bin/bash
# Regenerable caches: folders kept as nested subvolumes, outside automatic states and remote copies;
# carried for a live move with --caches.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cstate() { l overlay list | grep cache.target | cut -f6; }
cd $P/main
l config regenerable '["target"]'
l line new -q w
cd $P/w
ok "a new line gets the cache as a subvolume" sh -c "btrfs subvolume show $P/w/target >/dev/null"
head -c 20000000 /dev/urandom > target/big.bin; echo src >> notes.txt
l save -q
eq "automatic states do not hold the cache content" "" "$(l show head@{0}:target | tail -n +3)"
eq "status ignores the cache" " M notes.txt" "$(l status -s)"
l save -q --caches
eq "save --caches makes the cache overlay" "cache" "$(cstate)"
rm -rf target; mkdir target; echo rebuilt > target/out
has "a tool replaced the cache with a plain folder" "plain folder" "$(cstate)"
l overlay repair cache.target >/dev/null; l save -q
eq "overlay repair makes it a cache again" "cache" "$(cstate)"
eq "and keeps its content" "rebuilt" "$(cat target/out)"
# A live move with warm build output. The first sync sends the state without caches; the next
# sync with --caches must still send them.
new_fs $T/b 4G
head -c 1000000 /dev/urandom > target/warm.bin
# The regenerable setting belongs to this machine: .layrattributes makes target a cache on the peer too.
echo 'target regenerable' > .layrattributes
l add .layrattributes; l save -q -m "target is a cache"
l save -q --caches
# Caches go only to machines the project trusts (docs/cache-overlays.md).
peer_self=$(env LAYR_ROOT=$T/b/store $LAYR remote self --json)
l remote add test-peer ssh://test/g >/dev/null
ok "trust the peer" l remote trust test-peer "$(echo "$peer_self" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')" \
    --public-key "$(echo "$peer_self" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')"
peer_exchange sync g -q --auto -- env LAYR_ROOT=$T/b/store $LAYR serve-peer g
peer_exchange sync g -q --auto --caches -- env LAYR_ROOT=$T/b/store $LAYR serve-peer g
env LAYR_ROOT=$T/b/store $LAYR --project g adopt w >/dev/null
eq "the moved line has the build output" "$(md5sum < target/warm.bin)" "$(md5sum < $T/b/store/projects/g/lines/w/target/warm.bin)"
ok "and it is a cache there" sh -c "btrfs subvolume show $T/b/store/projects/g/lines/w/target >/dev/null"
finish
