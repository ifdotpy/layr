#!/bin/bash
# ci: macos overlayfs
# Removed-overlay state discovery happens before the repair lock.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
printf 'target regenerable\n' > "$T/src/.layrattributes"
printf 'target/\n' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
mkdir -p target
printf cached > target/file
l save --caches -q
l overlay remove cache.target
O=$P/meta/overlays/cache.target
before=$(ls "$O/states" | wc -l | tr -d ' ')
ok "overlay has states to remove" test "$before" -gt 1
mkdir -p "$T/wait"
touch "$T/wait/go.2" "$T/wait/go.3"
LAYR_TEST_REPAIR_WAIT="$T/wait" LAYR_TRACE=1 l gc --repair -q 2>"$T/repair.err" &
pid=$!
for _ in $(seq 1 600); do [ -e "$T/wait/planned.1" ] && break; sleep 0.1; done
ok "repair planned" test -e "$T/wait/planned.1"
ok "foreground status runs while planning waits" l status
touch "$T/wait/go.1"
wait "$pid"
eq "repair succeeds" 0 "$?"
eq "only the root state remains" 1 "$(ls "$O/states" | wc -l | tr -d ' ')"
ok "owners match" l gc -q
# Recreate the same overlay after planning: the overlay records change (not the parent records),
# so the removed-overlay check under the lock keeps the recreated overlay.
l save --caches -q
l overlay remove cache.target
rm -rf "$T/wait" && mkdir -p "$T/wait"
touch "$T/wait/go.2" "$T/wait/go.3"
LAYR_TEST_REPAIR_WAIT="$T/wait" LAYR_TRACE=1 l gc --repair -q 2>"$T/retry.err" &
pid=$!
for _ in $(seq 1 600); do [ -e "$T/wait/planned.1" ] && break; sleep 0.1; done
ok "second repair planned" test -e "$T/wait/planned.1"
printf new > target/file
ok "foreground recreates removed overlay" l save --caches -q
touch "$T/wait/go.1"
wait "$pid"
eq "repair succeeds after the overlay was recreated" 0 "$?"
l overlay list > "$T/overlays"
ok "recreated overlay survives" grep -q cache.target "$T/overlays"
ok "owners match after recreation" l gc -q
ok "project lock trace has a sample" grep -q 'store lock held: p$' "$T/repair.err"
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' "$T/repair.err")
ok "project lock held less than 2 seconds ($held ms)" test "$held" -lt 2000
finish
