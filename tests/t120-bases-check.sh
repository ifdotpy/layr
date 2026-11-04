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
finish
