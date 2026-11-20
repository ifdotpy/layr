#!/bin/bash
# ci: macos overlayfs
# Empty replacement caches start cold; unknown bytes remain intact and unlabelled.
. "$(dirname "$0")/lib.sh"
# Keep move destinations on the test filesystem, outside the store.
FS=$LAYR_ROOT
export LAYR_ROOT=$FS/store
mkdir -p "$LAYR_ROOT"
sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init g --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd "$P/main"
l line new -q w >/dev/null
cd "$P/w"
echo one >> notes.txt
l save -q
cp target/.layr-cache-location "$T/provenance"
rm -rf target; mkdir target
ok "an automatic save accepts an empty replacement cache" l save -q
ok "the empty replacement gets this location's provenance" cmp "$T/provenance" target/.layr-cache-location
rm -rf target; mkdir target
echo staged >> notes.txt
l add notes.txt >/dev/null || exit 1
ok "a named save accepts an empty replacement cache" l save -qm 'save with an empty cache'
ok "the named save initializes cold provenance" cmp "$T/provenance" target/.layr-cache-location
rm -rf target; mkdir target
ok "an explicit overlay save accepts an empty replacement cache" l save --overlays -q
ok "the overlay save initializes cold provenance" cmp "$T/provenance" target/.layr-cache-location
rm -rf target; mkdir -p target; echo big > target/x
out=$(l save -q 2>&1); rc=$?
no "a recreated cache with unknown bytes is refused" test "$rc" = 0
has "the refusal keeps output intact" 'output left intact' "$out"
eq "the file remains in the recreated folder" big "$(cat target/x)"
no "a save never stamps the unknown replacement" test -e target/.layr-cache-location
rm -rf target; mkdir -p target; echo big2 > target/y
out=$(l save --overlays -q 2>&1); rc=$?
no "an explicit overlay save also refuses unknown replacement output" test "$rc" = 0
eq "the second file is kept" big2 "$(cat target/y)"
no "the second replacement is not stamped" test -e target/.layr-cache-location
rm -rf target; mkdir -p target/empty-child
out=$(l save -q 2>&1); rc=$?
no "an empty child still makes a replacement non-empty" test "$rc" = 0
ok "the child directory remains intact" test -d target/empty-child
no "the non-empty directory is not stamped" test -e target/.layr-cache-location
rm -rf target; mkdir target; echo hidden > target/.hidden
out=$(l save -q 2>&1); rc=$?
no "hidden output is also refused" test "$rc" = 0
eq "hidden output remains intact" hidden "$(cat target/.hidden)"
no "hidden output is not stamped" test -e target/.layr-cache-location

# Move preflight must leave initialization to validation at the final build location.
rm -rf target; mkdir target
cd "$T"
ok "an empty replacement permits a physical move" l --project g line move w "$FS/moved"
ok "the moved cache has provenance for the final location" python3 - "$FS/moved" <<'PY'
import json, pathlib, sys
workspace = pathlib.Path(sys.argv[1])
marker = json.loads((workspace / 'target/.layr-cache-location').read_text())
assert marker['location'] == {'workspace': str(workspace), 'target': str(workspace / 'target')}
PY
finish
