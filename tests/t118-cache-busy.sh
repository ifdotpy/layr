#!/bin/bash
# ci: overlayfs
# A cache with an active writer and missing provenance is refused before conversion or deletion.
if [ "$(uname -s)" != Linux ]; then
    echo "t118-cache-busy.sh (needs Linux): 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init cb --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/cb/lines
cd "$P/main"
l config regenerable '["target"]'
l line new -q w >/dev/null
cd "$P/w"
mkdir -p target/debug
cp target/.layr-cache-location "$T/provenance"
rm target/.layr-cache-location
(exec 3>target/debug/.cargo-lock; while :; do date >> target/debug/out; sleep .05; done) &
writer=$!
trap 'if [ -n "${writer:-}" ]; then kill "$writer" 2>/dev/null || true; wait "$writer" 2>/dev/null || true; fi; cleanup' EXIT
sleep .2
inode=$(stat -c %i target)
size=$(stat -c %s target/debug/out)
out=$(l save -q 2>&1); rc=$?
no "an automatic save refuses unproven active output" test "$rc" = 0
has "the refusal explains that exclusion is unavailable" 'cannot exclude active builds' "$out"
out=$(l save --overlays --folder target -q 2>&1); rc=$?
no "an explicit save also refuses the active reset" test "$rc" = 0
eq "the active folder keeps its inode" "$inode" "$(stat -c %i target)"
no "neither save stamps the active output" test -e target/.layr-cache-location
sleep .2
ok "the writer continues at its original path" test "$(stat -c %s target/debug/out)" -gt "$size"
kill "$writer" 2>/dev/null || true
wait "$writer" 2>/dev/null || true
writer=""
# Restoring this fixture's original, same-location marker is explicit; saved history is never inferred.
cp "$T/provenance" target/.layr-cache-location
ok "known provenance remains usable after the writer stops" l save --overlays -q
finish
