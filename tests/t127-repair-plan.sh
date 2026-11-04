#!/bin/bash
# ci: macos overlayfs
# gc --repair reads the records and the overlays before the project lock (#194). A save and a line
# delete that run while it waits after its plan (LAYR_TEST_REPAIR_WAIT) are not blocked, and
# the repair retries its plan outside the lock: it keeps the new cache state and drops the cache lines
# of the deleted line.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf 'target regenerable\nbuild regenerable\n' > $T/src/.layrattributes
printf 'build/\n' >> $T/src/.gitignore
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/overlays
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
states() { ls "$O/$1/states" | wc -l | tr -d ' '; }

# Three lines with two cache states each in two cache overlays (history=last keeps the newest).
for n in a b c; do
    l line new -q $n main >/dev/null
    cd $P/$n || exit 1
    mkdir -p target build
    echo $n.1 > target/x; echo $n.1 > build/y
    l save --caches -q
    echo $n.2 > target/x; echo $n.2 > build/y
    l save --caches -q
done
cd $P/main || exit 1
l gc --repair -q >/dev/null 2>&1
before_t=$(states cache.target)
before_b=$(states cache.build)

# The repair waits after each plan N (planned.N) until the test writes go.N.
W=$T/wait
planned() { # planned <n>: wait until the repair has made plan n
    for _ in $(seq 1 600); do [ -e "$W/planned.$1" ] && return 0; sleep 0.1; done
    echo "FATAL: the repair made no plan $1" >&2; kill $pid 2>/dev/null; exit 1
}

# After plan 1, line a saves a new cache and line c is deleted.
mkdir -p $W && touch $W/go.2 $W/go.3
LAYR_TEST_REPAIR_WAIT=$W LAYR_TRACE=1 l gc --repair -q 2>$T/repair.err &
pid=$!
planned 1
cd $P/a || exit 1
echo a.3 > target/x; echo a.3 > build/y
l save --caches -q
cd $P/main || exit 1
l line delete -D c >/dev/null 2>&1
ok "the save and the line delete finish while the repair waits" kill -0 $pid
touch $W/go.1
wait $pid
eq "the repair succeeds" 0 "$?"
ok "the changed project makes a second plan" test "$(grep -c "trace begin repair: plan" $T/repair.err)" -ge 2

# The wait is not under the project lock.
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' $T/repair.err)
ok "the repair holds the project lock less than 2 s ($held ms)" test "$held" -lt 2000

# A stale plan causes a second replay, after the first lock was released. The trace's
# begin and end markers bound each replay, so no project-lock release can occur inside it.
ok "the stale plan is retried outside the project lock" awk '
    /trace begin repair: plan/ { if (planning || (plans && !released)) exit 1; planning=1; plans++; released=0 }
    /ms  repair: plan$/ { if (!planning) exit 1; planning=0 }
    /store lock held: p$/ { if (planning) exit 1; released=1 }
    END { if (plans < 2 || planning) exit 1 }
' "$T/repair.err"

# The repair used the project as it was under the lock: the new cache of a stays, its old one and
# the caches of c go.
l line new -q a2 a >/dev/null
eq "a new line from a has the newest target cache" a.3 "$(cat $P/a2/target/x)"
eq "a new line from a has the newest build cache" a.3 "$(cat $P/a2/build/y)"
eq "the target overlay drops c and keeps one state of a" "$((before_t - 1))" "$(states cache.target)"
eq "the build overlay drops c and keeps one state of a" "$((before_b - 1))" "$(states cache.build)"
ok "the owners table matches the records" l gc -q

# A second repair with nothing new changes nothing.
l gc --repair -q >/dev/null 2>&1
eq "a second repair keeps the target states" "$((before_t - 1))" "$(states cache.target)"
# Invalidate every attempt. The explicit command must stop after three plans and tell the
# caller to retry, rather than doing a replay under the lock.
rm -rf $W && mkdir -p $W
LAYR_TEST_REPAIR_WAIT=$W LAYR_TRACE=1 l gc --repair -q 2>$T/busy.err &
pid=$!
for n in 1 2 3; do
    planned $n
    cd "$P/a" || exit 1
    echo "busy.$n" > notes.txt
    l save -q
    cd "$P/main" || exit 1
    touch $W/go.$n
done
wait "$pid" && rc=0 || rc=$?
ok "a busy project makes explicit repair fail" test "$rc" -ne 0
ok "the failure asks the caller to retry" grep -q 'project changed during the command; run it again' "$T/busy.err"
eq "repair stops after three plans" 3 "$(grep -c 'trace begin repair: plan' "$T/busy.err")"
ok "owners still match after skipped repair" l gc -q
finish
