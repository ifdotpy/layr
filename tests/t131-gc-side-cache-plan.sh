#!/bin/bash
# ci: macos overlayfs
# Side-cache scanning and replacement checks happen without holding the project lock.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
I=$P/git/layr-index
C=$P/meta/overlap-compares
mkdir -p "$I" "$C" "$T/wait"
printf old > "$I/stale"
printf old > "$I/replaced"
printf bad > "$C/stale"
printf bad > "$C/replaced"
touch "$T/wait/go.2" "$T/wait/go.3"
LAYR_TEST_REPAIR_WAIT="$T/wait" LAYR_TRACE=1 l gc --repair -q 2>"$T/repair.err" &
pid=$!
for _ in $(seq 1 600); do [ -e "$T/wait/planned.1" ] && break; sleep 0.1; done
ok "repair planned" test -e "$T/wait/planned.1"
ok "foreground status runs while planning waits" l status
mv "$I/replaced" "$T/old-index"
printf replacement > "$I/replaced"
mv "$C/replaced" "$T/old-compare"
printf replacement > "$C/replaced"
touch "$T/wait/go.1"
wait "$pid"
eq "repair succeeds" 0 "$?"
ok "stale index removed" test ! -e "$I/stale"
ok "stale compare removed" test ! -e "$C/stale"
eq "replacement index kept" replacement "$(cat "$I/replaced")"
eq "replacement compare kept" replacement "$(cat "$C/replaced")"
ok "owners match" l gc -q
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' "$T/repair.err")
ok "project lock held less than 2 seconds ($held ms)" test "$held" -lt 2000
finish
