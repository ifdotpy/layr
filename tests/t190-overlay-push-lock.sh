#!/bin/bash
# ci: macos overlayfs
# A store push of a cache overlay holds the overlay's project lock for its key records only: the upload runs under
# the lock of the remote folder, so a new line takes the cache while the push still runs.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
mkdir -p target && echo built > target/out && l save --caches -q
LAYR_TEST_OVERLAY_PUSH_MS=6000 "$LAYR" push archive -q > "$T/push.log" 2>&1 &
PUSH=$!
sleep 2
start=$(date +%s)
ok "a new line runs during the cache push" l line new -q during main
took=$(( $(date +%s) - start ))
ok "it did not wait for the push ($took s)" test "$took" -lt 4
eq "it took the cache" "built" "$(cat "$P/during/target/out" 2>/dev/null)"
wait $PUSH
eq "the push succeeded" "0" "$?"
finish
