#!/bin/bash
# A schema rebuild must not show empty derived tables to other commands (src/db.rs, Db::rebuild).
# The project is set to schema version 6. The first command rebuilds it and holds the rebuild
# transaction open for 5 s with LAYR_TEST_REBUILD_HOLD_MS (test only). Concurrent commands must
# each list both lines, exit 0 and never print "fatal".
. "$(dirname "$0")/lib.sh"

sample $T/src
l init s --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/s/lines/main || exit 1
l line new -q w

db=$LAYR_ROOT/projects/s/meta/db/layr.sqlite
[ -f "$db" ] || db=$LAYR_ROOT/projects/s/meta/layr.sqlite
sqlite3 "$db" "PRAGMA user_version = 6"
eq "project is at schema 6" "6" "$(sqlite3 "$db" 'PRAGMA user_version')"

out=$T/rebuild-out
mkdir -p "$out"
# The rebuilder starts first and holds the rebuild; the readers start while it holds the lock.
( export LAYR_TEST_REBUILD_HOLD_MS=5000; l line list > "$out/holder.txt" 2>&1; echo $? > "$out/holder.rc" ) &
sleep 1
for i in 1 2 3 4; do
    ( l line list > "$out/r$i.txt" 2>&1; echo $? > "$out/r$i.rc" ) &
done
wait

for f in holder r1 r2 r3 r4; do
    eq "$f exits 0" "0" "$(cat "$out/$f.rc")"
    has "$f lists main" "main" "$(cat "$out/$f.txt")"
    has "$f lists w" "w" "$(cat "$out/$f.txt")"
    ok "$f has no fatal error" test -z "$(grep -i fatal "$out/$f.txt")"
done
has "a reader waited for the rebuild" "waiting for the database rebuild" "$(cat "$out/r1.txt" "$out/r2.txt" "$out/r3.txt" "$out/r4.txt")"
eq "project is at schema 7 after the rebuild" "7" "$(sqlite3 "$db" 'PRAGMA user_version')"
eq "rebuilt lines are listed" "2" "$(l line list | wc -l | tr -d ' ')"

finish
