#!/bin/bash
# ci: macos overlayfs
# Repair visits overlay temporary copies and never infers an owner's death from copied times.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
mkdir -p target
echo local > target/out.bin
l save --overlays -q
O=$P/meta/overlays/$TG
ok "the fixture has a cache overlay" test -f "$O/meta/db/layr.sqlite"
old_time=$(date -d '2 hours ago' +%Y%m%d%H%M 2>/dev/null || date -v-2H +%Y%m%d%H%M)
for root in "$P" "$O"; do
    for kind in scan work; do
        dir=$root/meta/$kind
        mkdir -p "$dir"
        for name in "11111111-2222-3333-4444-555555555556.$$" \
                    11111111-2222-3333-4444-555555555557.2000000000 \
                    .apfs-11111111-2222-3333-4444-555555555558 \
                    .apfs-11111111-2222-3333-4444-555555555559.2000000000; do
            mkdir "$dir/$name"
            echo unique > "$dir/$name/data"
            touch -t "$old_time" "$dir/$name"
        done
    done
done
# A legacy name with an open file has no creator suffix. Age must not delete it.
exec 8<"$O/meta/work/.apfs-11111111-2222-3333-4444-555555555558/data"
preview=$(l gc --repair -n 2>&1)
has "dry-run names the overlay temporary copy" "$O/meta/work/11111111-2222-3333-4444-555555555557.2000000000" "$preview"
has "dry-run explains the retained unknown creator" "creator unknown" "$preview"
for root in "$P" "$O"; do
    for kind in scan work; do
        ok "dry-run keeps $root/$kind" test -e "$root/meta/$kind/11111111-2222-3333-4444-555555555557.2000000000/data"
    done
done
ok "repair succeeds" l gc --repair
for root in "$P" "$O"; do
    for kind in scan work; do
        dir=$root/meta/$kind
        ok "repair keeps the live creator in $root/$kind" test -f "$dir/11111111-2222-3333-4444-555555555556.$$/data"
        ok "repair removes the dead creator in $root/$kind" test ! -e "$dir/11111111-2222-3333-4444-555555555557.2000000000"
        ok "repair keeps an unknown creator in $root/$kind" test -f "$dir/.apfs-11111111-2222-3333-4444-555555555558/data"
        ok "repair removes a dead APFS copy in $root/$kind" test ! -e "$dir/.apfs-11111111-2222-3333-4444-555555555559.2000000000"
    done
done
exec 8<&-
# The all-projects path uses the same cleanup, even though overlays are not top-level projects.
dead_copy=$(bash -c 'path="$1/11111111-2222-3333-4444-555555555560.$$"; mkdir "$path" || exit; printf "%s" "$path"' bash "$O/meta/work") || exit 1
[ -n "$dead_copy" ] || exit 1
ok "all-projects repair succeeds" l gc --all-projects -q
ok "all-projects removes abandoned overlay work" test ! -e "$dead_copy"
finish
