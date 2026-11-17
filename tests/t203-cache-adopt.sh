#!/bin/bash
# ci: macos overlayfs
# A peer trusted after the cache save receives that cache, and adoption at another location starts cold.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init cachemove --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/cachemove/lines/main"
l line new -q moving
cd "$LAYR_ROOT/projects/cachemove/lines/moving"
mkdir -p target
echo 'saved build output' > target/output
l save -q --overlays
new_fs "$T/peer" 2G
peer_id=$(env LAYR_ROOT="$T/peer/store" "$LAYR" remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
out=$(peer_exchange sync cachemove -q --auto -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer cachemove 2>&1); rc=$?
eq "sync without caches trusts the new peer" 0 "$rc"
[ "$rc" = 0 ] || printf '%s\n' "$out"
out=$(peer_exchange sync cachemove -q --auto -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer cachemove 2>&1); rc=$?
eq "a later sync transfers caches saved before peer trust" 0 "$rc"
[ "$rc" = 0 ] || printf '%s\n' "$out"
has "the peer is now a cache reader" "machine:$peer_id" "$(l overlay readers $TG)"
ok "adopt on the peer" env LAYR_ROOT="$T/peer/store" "$LAYR" --project cachemove adopt moving
peer_line="$T/peer/store/projects/cachemove/lines/moving"
no "adopt rejects output built at the source location" test -e "$peer_line/target/output"
echo 'private rebuild' > "$peer_line/target/output"
eq "the source cache stays independent" 'saved build output' "$(cat target/output)"
eq "peer cache writes stay outside source status" "" "$(cd "$peer_line" && env LAYR_ROOT="$T/peer/store" "$LAYR" status --short)"

finish
