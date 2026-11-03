#!/bin/bash
# Freeing by owners (docs/ownership.md): a state is freed by the command that drops its last
# owner. Covers the line delete, the count rings (retention.auto, undo.ops, undo.deleted), the
# crash between commit and move, a pinned read, the removed time keys and gc check and repair.
. "$(dirname "$0")/lib.sh"

sample $T/src
# newproj <name>: a project with its main line as the working folder.
newproj() {
    l init "$1" --from $T/src -q >/dev/null || exit 1
    P=$LAYR_ROOT/projects/$1
    DB=$P/meta/db/layr.sqlite
    [ -f "$DB" ] || DB=$P/meta/layr.sqlite
    M=$P/lines
    cd $M/main || exit 1
}
q() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute(sys.argv[2]).fetchone()[0])' "$DB" "$1"; }
exec_sql() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(sys.argv[2]); c.commit()' "$DB" "$1"; }
line_id() { q "select id from lines where name='$1' and deleted=0"; }
# A state has its storage when its tree or its thin layer is on disk.
on_disk() { [ -e "$P/states/$1" ] || [ -e "$P/deltas/$1" ]; }
present_of_id() { q "select count(*) from states where line='$1' and present=1"; }
first_auto() { q "select id from states where kind='auto' and line='$(line_id "$1")' order by time, id limit 1"; }
newest_auto() { q "select id from states where kind='auto' and line='$(line_id "$1")' order by time desc, id desc limit 1"; }
queue() { q "select count(*) from free_queue"; }

# 1. A line delete frees the line's unique states in that command, and the record stays dead.
newproj o
l line new -q w >/dev/null
cd $M/w || exit 1
for i in 1 2 3 4 5 6; do echo "auto $i" > auto.txt; l save -q; done
wid=$(line_id w)
before=$(present_of_id "$wid")
ok "the line has automatic states before its delete" test "$before" -ge 6
oldest=$(first_auto w)
ok "the oldest automatic state has storage" on_disk "$oldest"
cd $M/main || exit 1
l line delete -D w >/dev/null
eq "the line delete leaves only its final state present" 1 "$(present_of_id "$wid")"
no "the oldest automatic state of the deleted line has no storage" on_disk "$oldest"
eq "the freed state keeps its record as dead" 1 "$(q "select count(*) from states where id='$oldest' and present=0 and deleted=1")"
eq "the free queue is empty after the command" 0 "$(queue)"

# 2. undo of the delete works while the line is in the undo.deleted ring.
l undo >/dev/null 2>&1
ok "undo of the delete restores the working folder" test -f "$M/w/auto.txt"
ok "undo of the delete restores the line" test "$(l line list | awk '{print $1}' | grep -cx w)" -eq 1

# 3. The auto ring: the 11th automatic save frees the oldest automatic state. undo.ops is 1 so
# that the undo ring does not keep the states of older saves.
newproj ring
l config undo.ops 1 >/dev/null
# Each automatic state is a thin layer over the one before (APFS). A short chain lets the
# dropped state go at once; a longer chain keeps it as a base (t: apfs-ownership-bases).
l config thin.chain 1 >/dev/null
l line new -q a >/dev/null
cd $M/a || exit 1
for i in $(seq 1 10); do echo "ring $i" > ring.txt; l save -q; done
oldest=$(first_auto a)
ok "ten automatic states keep the oldest" on_disk "$oldest"
echo "ring 11" > ring.txt; l save -q
no "the 11th automatic save frees the oldest automatic state" on_disk "$oldest"
eq "ten automatic states stay owned" 10 "$(q "select count(*) from owners where kind='auto' and holder='$(line_id a)'")"

# 4. undo works within undo.ops; beyond the ring it says the state was freed.
newproj uo
l config retention.auto 1 >/dev/null
l config undo.ops 2 >/dev/null
l line new -q u >/dev/null
cd $M/u || exit 1
for i in 1 2 3 4; do echo "u $i" >> u.txt; l add u.txt >/dev/null; l save -qm "u $i"; done
# Three resets: the first one is past undo.ops (2) and names records of the line that are gone.
for _ in 1 2 3; do l reset -q --hard head~1; done
old_op=$(l op log --all -n 200 | grep ' reset ' | tail -1 | awk '{print $1}')
has "undo beyond undo.ops says it cannot be undone" "cannot undo" "$(l undo "$old_op" 2>&1 || true)"
cd $M/main || exit 1
new_op=$(l op log -n 1 | awk '{print $1}')
ok "the newest operation can still be undone" l undo -n "$new_op"

# 5. A crash between the commit and the move leaves the queue; the next command finishes it.
newproj cr
l line new -q c >/dev/null
cd $M/c || exit 1
for i in 1 2 3; do echo "c $i" > c.txt; l save -q; done
c_old=$(first_auto c)
cd $M/main || exit 1
LAYR_TEST_EXIT_AFTER_COMMIT=1 "$LAYR" line delete -D c >/dev/null 2>&1
eq "the crash exits with status 86" 86 "$?"
ok "the crash leaves states in the queue" test "$(queue)" -gt 0
l status >/dev/null 2>&1
eq "the next command empties the queue" 0 "$(queue)"
no "the next command frees the states of the crashed command" on_disk "$c_old"

# 6. A pinned read keeps its state until the read ends. `try` pins the state it runs on.
newproj pn
l config undo.ops 1 >/dev/null
l config retention.auto 1 >/dev/null
l config thin.chain 1 >/dev/null
l line new -q p >/dev/null
cd $M/p || exit 1
for i in 1 2 3; do echo "p $i" > p.txt; l save -q; done
pinned=$(newest_auto p)
(cd $M/p && "$LAYR" try "$pinned" -- sleep 4 >/dev/null 2>&1) &
for _ in $(seq 1 60); do
    [ "$(q "select count(*) from owners where kind='pin' and state='$pinned'")" -gt 0 ] && break
    sleep 0.1
done
for i in 4 5 6; do echo "p $i" > p.txt; l save -q; done
ok "a pinned state keeps its storage while the read runs" on_disk "$pinned"
wait
no "a pinned state goes when the read ends" on_disk "$pinned"
eq "no pin is left after the read" 0 "$(q "select count(*) from owners where kind='pin'")"

# 7. The time keys are removed; the message names the replacement.
newproj o2
has "undo.days is removed and the message names undo.ops" "undo.ops" "$(l config undo.days 3 2>&1 || true)"
no "undo.days cannot be set" l config undo.days 3
has "retention.hours is removed and the message names retention.auto" "retention.auto" "$(l config retention.hours 3 2>&1 || true)"
no "retention.hours cannot be set" l config retention.hours 3

# 8. gc check: exit 0 on a clean store, exit 1 with a forced leak, gc --repair fixes it.
l line new -q x >/dev/null
eq "gc check exits 0 on a clean store" 0 "$(l gc >/dev/null 2>&1; echo $?)"
echo x >> notes.txt; l save -qam c1 >/dev/null
parent=$(q "select state from owners where kind='parent' limit 1")
exec_sql "delete from owners where state='$parent'"
has "gc names the leak and what the repair frees" "would be freed by" "$(l gc 2>&1 || true)"
no "gc check exits 1 with a forced leak" l gc
l gc --repair >/dev/null 2>&1
eq "gc check exits 0 after the repair" 0 "$(l gc >/dev/null 2>&1; echo $?)"
eq "fsck finds no problem after the repair" "0 problem(s)" "$(l fsck | tail -1)"

# 9. Leaked states (present, no owner) are not freed by a normal operation. gc reports them, and
# --repair frees them. The leaks are made here: the storage of freed states, marked present.
newproj lk
l line new -q w >/dev/null
cd $M/w || exit 1
for i in 1 2 3; do echo "lk $i" > lk.txt; l save -q; done
wid=$(line_id w)
cd $M/main || exit 1
l line delete -D w >/dev/null
pre=$(q "select count(*) from states where present=1")
exec_sql "update states set present=1, deleted=0 where present=0 and deleted=1 and kind='auto' and line='$wid'"
leaked=$(( $(q "select count(*) from states where present=1") - pre ))
ok "the forced leaks are present states" test "$leaked" -gt 0
echo "after" >> notes.txt; l save -qam "after the leaks" >/dev/null 2>&1
eq "a normal save frees no leak" "$(( pre + leaked ))" "$(( $(q "select count(*) from states where present=1") - 1 ))"
has "gc reports the leaked states" "leaks:" "$(l gc 2>&1 || true)"
after_save=$(q "select count(*) from states where present=1")
l gc --repair >/dev/null 2>&1
eq "gc --repair frees the leaked states" "$(( after_save - leaked ))" "$(q "select count(*) from states where present=1")"
eq "fsck finds no problem after the leaks are freed" "0 problem(s)" "$(l fsck | tail -1)"

finish
