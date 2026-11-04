#!/bin/bash
# ci: macos overlayfs
# gc --repair reads the records and the overlays before the project lock (#194). A save and a line
# delete that run while it waits after its plan (LAYR_TEST_REPAIR_PAUSE_MS) are not blocked, and
# the repair reads again under the lock: it keeps the new cache state and drops the cache lines
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

# The repair waits after its plan. Meanwhile line a saves a new cache and line c is deleted.
LAYR_TEST_REPAIR_PAUSE_MS=4000 LAYR_TRACE=1 l gc --repair -q 2>$T/repair.err &
pid=$!
sleep 1
cd $P/a || exit 1
echo a.3 > target/x; echo a.3 > build/y
l save --caches -q
cd $P/main || exit 1
l line delete -D c >/dev/null 2>&1
ok "the save and the line delete finish while the repair waits" kill -0 $pid
wait $pid
eq "the repair succeeds" 0 "$?"

# The pause is not under the project lock.
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' $T/repair.err)
ok "the repair holds the project lock less than its pause ($held ms)" test "$held" -lt 4000

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
finish
