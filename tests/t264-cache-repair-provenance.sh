#!/bin/bash
# ci: macos macos-plain overlayfs
# Repair initializes absent output at this location and refuses existing output of unknown origin.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init cache-repair --from "$T/src" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/cache-repair/lines/main"
l protect main --remove >/dev/null 2>&1 || true
echo disposable > target/output
ok "establish the saved cache overlay" l save --overlays --folder target -q
cp target/.layr-cache-location "$T/provenance"
rm -rf target
no "the live cache folder is absent" test -e target
out=$(l overlay repair "$TG" 2>&1); rc=$?
eq "repair of an absent cache succeeds" 0 "$rc"
has "repair reports the new cache" 'target: a cache again' "$out"
ok "repair establishes the exact location provenance" cmp "$T/provenance" target/.layr-cache-location
ok "the repaired cache can be saved immediately" l save --overlays --folder target -q

rm -rf target
mkdir target
ok "repair accepts an empty replacement cache" l overlay repair "$TG"
ok "repair initializes the empty cache's location" cmp "$T/provenance" target/.layr-cache-location

rm -rf target
mkdir target
echo unknown > target/output
out=$(l overlay repair "$TG" 2>&1); rc=$?
no "repair refuses existing unknown output" test "$rc" = 0
has "the refusal explains exclusion" 'cannot exclude active builds' "$out"
eq "repair preserves the unknown output" unknown "$(cat target/output)"
no "repair never stamps unknown output" test -e target/.layr-cache-location
finish
