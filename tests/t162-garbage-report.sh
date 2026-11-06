#!/bin/bash
# ci: macos
# The daemon repairs nothing (docs/ownership.md, "gc"). It writes a garbage report to its log: one line for each
# project where `layr gc` finds something, here a leaked state. The report changes nothing; `gc --repair` frees it.
# macOS only: there the daemon runs as the user.
. "$(dirname "$0")/lib.sh"
if [ "$(uname -s)" != Darwin ] || [ "$(id -u)" = 0 ]; then
    echo "$(basename "$0") (macOS user only): 0 passed, 0 failed"
    exit 0
fi

sample "$T/src"
l init lk --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/lk
DB=$P/meta/db/layr.sqlite
q() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute(sys.argv[2]).fetchone()[0])' "$DB" "$1"; }
exec_sql() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(sys.argv[2]); c.commit()' "$DB" "$1"; }
cd $P/lines/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q w >/dev/null
cd $P/lines/w || exit 1
for i in 1 2 3; do echo "lk $i" > lk.txt; l save -q; done
wid=$(q "select id from lines where name='w' and deleted=0")
cd $P/lines/main || exit 1
l line delete -D w >/dev/null
# A leak, as a bug would make it: the storage of freed states, marked present again.
exec_sql "update states set present=1, deleted=0 where present=0 and deleted=1 and kind='auto' and line='$wid'"
leaked=$(q "select count(*) from states where present=1")
no "gc finds the leak" l gc -q

"$LAYR" daemon --tick 1 --socket $T/d.sock > $T/daemon.log 2>&1 &
dpid=$!
for i in $(seq 1 60); do grep -q "garbage report (lk)" $T/daemon.log && break; sleep 0.5; done
# Stop the daemon after the pass it forked has ended (state Z: done).
kill -STOP $dpid 2>/dev/null
for i in $(seq 1 120); do
    busy=0
    for c in $(pgrep -P $dpid 2>/dev/null); do
        case "$(ps -o stat= -p "$c" 2>/dev/null)" in Z*|"") ;; *) busy=1 ;; esac
    done
    [ $busy -eq 0 ] && break
    sleep 0.5
done
kill -KILL $dpid 2>/dev/null; wait $dpid 2>/dev/null
out=$(cat $T/daemon.log)
has "the daemon reports the leak" "layr garbage report (lk): " "$out"
has "the report names the repair" "layr gc --repair" "$out"
eq "the report changes nothing" "$leaked" "$(q "select count(*) from states where present=1")"
l gc --repair -q >/dev/null 2>&1
ok "gc --repair frees the leak" l gc -q
finish
