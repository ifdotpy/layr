#!/bin/bash
# Read and write stores with a previous binary and the candidate binary.
: "${OLD_LAYR:?Set OLD_LAYR to the previous Linux binary}"
. "$(dirname "$0")/lib.sh"
sample "$T/src"
"$OLD_LAYR" init compat --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/compat/lines/main
cd "$P" || exit 1
old_head=$("$OLD_LAYR" rev-parse HEAD)
eq "candidate keeps the old state identifier" "$old_head" "$(l rev-parse head)"
ok "candidate validates old signatures and snapshots" l fsck
printf 'candidate change\n' >> notes.txt
l save -qam candidate
new_head=$(l rev-parse head)
eq "old binary reads the new state identifier" "$new_head" "$("$OLD_LAYR" rev-parse HEAD)"
ok "old binary validates new signatures and snapshots" "$OLD_LAYR" fsck
"$OLD_LAYR" commit --allow-empty -qm previous
ok "candidate accepts a later old-binary commit" l fsck
# Both protocol roles must work when only one binary knows the new capabilities.
new_fs "$T/peer-old" 2G
ok "new client sends to old peer" peer_exchange sync compat -q -- env LAYR_ROOT=$T/peer-old/store "$OLD_LAYR" serve-peer compat
no "a one-way push refuses a peer without direction support" peer_exchange push compat -q -- env LAYR_ROOT=$T/peer-old/store "$OLD_LAYR" serve-peer compat
eq "old peer has the same head" "$(l rev-parse head)" "$(LAYR_ROOT=$T/peer-old/store "$OLD_LAYR" --project compat --line main rev-parse HEAD)"
new_fs "$T/peer-new" 2G
ok "old client sends to new peer" "$OLD_LAYR" sync -q -- env LAYR_ROOT=$T/peer-new/store "$LAYR" serve-peer compat
eq "new peer has the same head" "$(l rev-parse head)" "$(LAYR_ROOT=$T/peer-new/store "$LAYR" --project compat --line main rev-parse head)"
# The CLI name changes to cache, but older stores and peers must keep warm data.
"$OLD_LAYR" layer add target
printf 'old build output\n' > target/old
"$OLD_LAYR" save -q --layers
l line new -q read-old-cache
ok "new client restores an old saved cache" cmp target/old ../read-old-cache/target/old
l config regenerable '["generated"]'
printf 'new build output\n' > generated/new
l save -q --caches
"$OLD_LAYR" branch -q read-new-cache
ok "old client restores a new saved cache" cmp generated/new ../read-new-cache/generated/new
ok "new client sends caches to old peer" peer_exchange sync compat -q --auto --caches -- env LAYR_ROOT=$T/peer-old/store "$OLD_LAYR" serve-peer compat
LAYR_ROOT=$T/peer-old/store "$OLD_LAYR" --project compat adopt main >/dev/null
ok "old peer restores the new cache content" cmp generated/new "$T/peer-old/store/projects/compat/lines/main/generated/new"
ok "old client sends caches to new peer" "$OLD_LAYR" sync -q --auto --layers -- env LAYR_ROOT=$T/peer-new/store "$LAYR" serve-peer compat
LAYR_ROOT=$T/peer-new/store l --project compat adopt main >/dev/null
ok "new peer restores the old cache content" cmp target/old "$T/peer-new/store/projects/compat/lines/main/target/old"
ok "new peer restores the new cache content" cmp generated/new "$T/peer-new/store/projects/compat/lines/main/generated/new"

finish
