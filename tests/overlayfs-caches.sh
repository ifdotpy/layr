#!/bin/bash
# Cache content, snapshots and transfer on an isolated OverlayFS test store.
: "${LAYR_OVERLAYFS_TEST_ROOT:?Set an isolated test directory}"
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init cachetest --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/cachetest/lines
cd "$P/main" || exit 1
l config regenerable '["target"]'
mkdir -p target
no "the cache is not a mount" mountpoint -q target
printf built > target/output
l save -q --glasses
l line new -q warm
eq "the new line has the saved cache" built "$(cat "$P/warm/target/output")"
ok "the new cache is a mount over the saved state (no copy)" mountpoint -q "$P/warm/target"
printf changed > "$P/warm/target/output"
eq "the source cache remains unchanged" built "$(cat target/output)"
eq "cache changes do not enter status" "" "$(cd "$P/warm" && l status -s)"
no "history excludes the cache output" l show head:target/output
# A build can remove and recreate the cache root.
ok "remove the cache root" rm -rf target
mkdir target
printf rebuilt > target/output
l save -q --glasses
eq "save --glasses preserves rebuilt output" rebuilt "$(cat target/output)"
# A saved cache belongs to a state. Advance the state before saving new output.
printf '\nnew build input\n' >> notes.txt
l save -qam "new build input"
l save -q --glasses
# The regenerable setting belongs to this machine: a .layrattributes file makes target a cache path on the peer too.
echo 'target regenerable' > .layrattributes
l add .layrattributes
l save -q -m "target is a cache"
l save -q --glasses
new_fs "$T/peer" 2G
# Caches go only to machines the project trusts (docs/cache-overlays.md): trust the peer first.
peer_self=$(env LAYR_ROOT="$T/peer/store" "$LAYR" remote self --json)
peer_id=$(echo "$peer_self" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
peer_key=$(echo "$peer_self" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote add test-peer ssh://test/cachetest >/dev/null
ok "trust the peer" l remote trust test-peer "$peer_id" --public-key "$peer_key"
ok "sync transfers saved caches" peer_exchange sync cachetest -q -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer cachetest
env LAYR_ROOT="$T/peer/store" "$LAYR" --project cachetest adopt main >/dev/null
eq "the peer restores the saved cache" rebuilt "$(cat "$T/peer/store/projects/cachetest/lines/main/target/output")"
ok "the peer cache is a mount over the saved state" mountpoint -q "$T/peer/store/projects/cachetest/lines/main/target"
ok "snapshots and signatures remain valid" l fsck
finish
