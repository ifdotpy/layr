#!/bin/bash
# ci: macos overlayfs
# A new line can make an old cache a head after retention planned its deletion. The repair
# must replan outside the project lock, keeping that cache while trimming other old caches.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
printf 'target regenerable\n' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
l config retention.caches 0 >/dev/null
for n in $(seq 1 10); do
    mkdir -p target
    echo "cache-$n" > target/value
    l save --caches -q
    echo "$n" > revision
    l add revision
    l save -qm "revision $n"
    [ "$n" = 1 ] && old=$(l rev-parse head~1)
done
before=$(find "$P/states" -name '*@*' -prune | wc -l | tr -d ' ')
LAYR_TEST_RETENTION_PAUSE_MS=4000 LAYR_TRACE=1 l gc --repair -q 2>"$T/repair.err" &
pid=$!
# The repair first runs the regular GC pass, then waits at the retention plan.
for _ in $(seq 1 100); do
    grep -q 'retention: planned' "$T/repair.err" && break
    sleep .1
done
ok "repair is waiting outside the lock" kill -0 "$pid"
ok "retention reached the planned pause" grep -q 'retention: planned' "$T/repair.err"
ok "new head is created while retention waits" l line new -q rescue "$old"
wait "$pid"
eq "repair succeeds after replanning" 0 "$?"
eq "new head keeps its warm cache" cache-1 "$(cat "$P/lines/rescue/target/value")"
after=$(find "$P/states" -name '*@*' -prune | wc -l | tr -d ' ')
ok "unneeded caches are trimmed" test "$after" -lt "$before"
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' "$T/repair.err")
ok "the pause is outside the project lock ($held ms)" test "$held" -lt 4000
ok "owners remain consistent" l gc -q
finish
