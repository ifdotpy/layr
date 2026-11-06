#!/bin/bash
# ci: macos overlayfs
# The bases table (layer state to base) must match the layers on disk. gc reports each difference,
# and gc --repair makes the table match the disk.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init b --from $T/src -q >/dev/null
db=$LAYR_ROOT/projects/b/meta/db/layr.sqlite
[ -f "$db" ] || db=$LAYR_ROOT/projects/b/meta/layr.sqlite
cd $LAYR_ROOT/projects/b/lines/main || exit 1

eq "a store without layers has no difference" "layer bases: 0 difference(s), differ" "$(l gc -n 2>&1 | grep 'layer bases')"

# A row that names a layer that is not on disk: a difference.
sqlite3 "$db" "INSERT INTO bases(state, base) VALUES('01a1ffff-0000-7000-8000-000000000001', '01a1ffff-0000-7000-8000-000000000002')"
eq "gc -n reports the stale row" "1" "$(l gc -n 2>&1 | grep -c 'stale row')"
eq "gc -n leaves the row" "1" "$(sqlite3 "$db" 'SELECT COUNT(*) FROM bases')"

l gc --repair -q >/dev/null 2>&1
eq "gc --repair removes the stale row" "0" "$(sqlite3 "$db" 'SELECT COUNT(*) FROM bases')"
eq "after repair there is no difference" "0" "$(l gc -n 2>&1 | grep 'layer bases' | awk '{print $3}')"
# Saves make thin layers: every layer gets its row when the state is recorded.
cd $LAYR_ROOT/projects/b/lines/main || exit 1
for i in 1 2 3; do echo "$i" > saved.txt; l save -q; done
eq "after saves the table names every layer" "layer bases: 0 difference(s), differ" "$(l gc -n 2>&1 | grep 'layer bases')"

# A layer made while gc --repair waits after its read of the disk keeps its row: the repair changes
# only the rows that differed in that read.
sqlite3 "$db" "INSERT INTO bases(state, base) VALUES('01a1ffff-0000-7000-8000-000000000003', '01a1ffff-0000-7000-8000-000000000004')"
LAYR_TEST_GC_PAUSE_MS=3000 l gc --repair -q >/dev/null 2>&1 &
pid=$!
sleep 1
echo 4 > saved.txt; l save -q
wait $pid
eq "the repair succeeds" 0 "$?"
eq "a layer made during the repair keeps its row" "layer bases: 0 difference(s), differ" "$(l gc -n 2>&1 | grep 'layer bases')"

finish
