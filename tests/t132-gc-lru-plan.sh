#!/bin/bash
# ci: macos overlayfs
# LRU scans before the repair lock, then trims unchanged files with the line lock only.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
printf 'target regenerable\n' > "$T/src/.layrattributes"
printf 'target/\n' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
mkdir -p target
printf seed > target/seed
l save --caches -q
l overlay set cache.target --gc lru:1 >/dev/null
printf old > target/stale
printf old > target/replaced
touch -t 200001010000 target/stale target/replaced
mkdir -p "$T/wait"
touch "$T/wait/go.2" "$T/wait/go.3"
LAYR_TEST_REPAIR_WAIT="$T/wait" LAYR_TRACE=1 l gc --repair -q 2>"$T/repair.err" &
pid=$!
for _ in $(seq 1 600); do [ -e "$T/wait/planned.1" ] && break; sleep 0.1; done
ok "repair planned" test -e "$T/wait/planned.1"
ok "foreground status runs while planning waits" l status
mv target/replaced "$T/previous"
printf replacement > target/replaced
printf new > target/new
touch "$T/wait/go.1"
wait "$pid"
eq "repair succeeds" 0 "$?"
ok "unchanged old cache removed" test ! -e target/stale
eq "replacement kept" replacement "$(cat target/replaced)"
eq "new file kept" new "$(cat target/new)"
ok "owners match" l gc -q
ok "project lock trace has a sample" grep -q 'store lock held: p$' "$T/repair.err"
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' "$T/repair.err")
ok "project lock held less than 2 seconds ($held ms)" test "$held" -lt 2000
finish
