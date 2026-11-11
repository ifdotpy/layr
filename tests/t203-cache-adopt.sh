#!/bin/bash
# ci: macos overlayfs
# A peer trusted after the cache save receives that cache, and adopt restores it.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init cachemove --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/cachemove/lines/main"
l line new -q moving
cd "$LAYR_ROOT/projects/cachemove/lines/moving"
mkdir -p target
echo 'saved build output' > target/output
l save -q --caches
new_fs "$T/peer" 2G
peer_id=$(env LAYR_ROOT="$T/peer/store" "$LAYR" remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
ok "sync without caches trusts the new peer" peer_exchange sync cachemove -q --auto -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer cachemove
ok "a later sync transfers caches saved before peer trust" peer_exchange sync cachemove -q --auto -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer cachemove
has "the peer is now a cache reader" "machine:$peer_id" "$(l overlay readers $TG)"
ok "adopt on the peer" env LAYR_ROOT="$T/peer/store" "$LAYR" --project cachemove adopt moving
peer_line="$T/peer/store/projects/cachemove/lines/moving"
ok "adopt restores the saved bytes" cmp target/output "$peer_line/target/output"
echo 'private rebuild' > "$peer_line/target/output"
eq "the source cache stays independent" 'saved build output' "$(cat target/output)"
eq "peer cache writes stay outside source status" "" "$(cd "$peer_line" && env LAYR_ROOT="$T/peer/store" "$LAYR" status --short)"

finish
