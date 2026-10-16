#!/bin/bash
# Cache content, snapshots and transfer on an isolated OverlayFS test store.
: "${LAYR_OVERLAYFS_TEST_ROOT:?Set an isolated test directory}"
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init cachetest --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/cachetest/lines
cd "$P/main" || exit 1
l cache add target
no "the cache is not a mount" mountpoint -q target
printf built > target/output
l save -q --caches
l branch -q warm
eq "the new line has the saved cache" built "$(cat "$P/warm/target/output")"
no "the new cache is not a mount" mountpoint -q "$P/warm/target"
printf changed > "$P/warm/target/output"
eq "the source cache remains unchanged" built "$(cat target/output)"
eq "cache changes do not enter status" "" "$(cd "$P/warm" && l status -s)"
no "history excludes the cache output" l show HEAD:target/output
# A build can remove and recreate the cache root.
ok "remove the cache root" rm -rf target
mkdir target
printf rebuilt > target/output
l cache repair
eq "cache repair preserves rebuilt output" rebuilt "$(cat target/output)"
# A saved cache belongs to a state. Advance the state before saving new output.
printf '\nnew build input\n' >> notes.txt
l commit -qam "new build input"
l save -q --caches
new_fs "$T/peer" 2G
ok "sync transfers saved caches" l sync -q --caches -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer cachetest
env LAYR_ROOT="$T/peer/store" "$LAYR" --project cachetest adopt main >/dev/null
eq "the peer restores the saved cache" rebuilt "$(cat "$T/peer/store/projects/cachetest/lines/main/target/output")"
no "the peer cache is not mounted" mountpoint -q "$T/peer/store/projects/cachetest/lines/main/target"
ok "snapshots and signatures remain valid" l fsck
finish
