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
l config retention.caches 100 >/dev/null
l config retention.auto 100 >/dev/null
l config undo.ops 100 >/dev/null
snapshot_of() {
    python3 - "$P/meta/db/layr.sqlite" "$1" <<'PY'
import json, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
row = db.execute('SELECT rec FROM states WHERE display=?', (sys.argv[2],)).fetchone()
print(json.loads(row[0])['layers']['target'])
PY
}
for n in $(seq 1 10); do
    mkdir -p target
    echo "cache-$n" > target/value
    saved=$(l save --caches | awk '$1 == "main" {print $2}')
    if [ "$n" = 1 ]; then old=$saved; original="$P/states/$(snapshot_of "$saved")"; fi
    if [ "$n" = 2 ]; then obsolete="$P/states/$(snapshot_of "$saved")"; fi
    echo "$n" > revision
    l add revision
    l save -qm "revision $n"
done
ok "original cache snapshot exists before retention" test -e "$original"
ok "another cache snapshot exists before retention" test -e "$obsolete"
l config retention.caches 0 >/dev/null
before=$(ls -1 "$P/states" | grep -c '@' || true)
W=$T/wait
mkdir -p "$W"
touch "$W/go.2" "$W/go.3"
LAYR_TEST_RETENTION_WAIT=$W LAYR_TRACE=1 l gc --repair -q 2>"$T/repair.err" &
pid=$!
# The repair first runs the regular GC pass, then waits at the retention plan.
for _ in $(seq 1 600); do
    [ -e "$W/planned.1" ] && break
    sleep .1
done
ok "repair is waiting outside the lock" kill -0 "$pid"
ok "retention reached the planned pause" test -e "$W/planned.1"
ok "new head is created while retention waits" l line new -q rescue "$old"
touch "$W/go.1"
wait "$pid"
eq "repair succeeds after replanning" 0 "$?"
eq "new head keeps its warm cache" cache-1 "$(cat "$P/lines/rescue/target/value")"
ok "the original cache snapshot stays" test -e "$original"
no "a separate obsolete snapshot goes" test -e "$obsolete"
ok "the changed project caused a replan" test -e "$W/planned.2"
after=$(ls -1 "$P/states" | grep -c '@' || true)
ok "unneeded caches are trimmed" test "$after" -lt "$before"
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' "$T/repair.err")
ok "the wait is outside the project lock ($held ms)" test "$held" -lt 2000
ok "owners remain consistent" l gc -q

# Repeated changes defer this optional cleanup; the repair itself must still succeed.
W=$T/busy
mkdir -p "$W"
LAYR_TEST_RETENTION_WAIT=$W l gc --repair -q 2>"$T/busy.err" &
pid=$!
for n in 1 2 3; do
    for _ in $(seq 1 600); do
        [ -e "$W/planned.$n" ] && break
        sleep .1
    done
    ok "retention planned attempt $n" test -e "$W/planned.$n"
    ok "change the project on attempt $n" l config "test.retention-$n" "$n"
    touch "$W/go.$n"
done
wait "$pid"
eq "busy retention does not fail repair" 0 "$?"
has "busy retention reports deferred cleanup" 'cleanup deferred' "$(cat "$T/busy.err")"
finish
