#!/bin/bash
# ci: macos overlayfs
# Storage of deleted lines (issue #192, part 1; docs/ownership.md, rule 4). A deleted line keeps no
# undo owner of its earlier operations. The undo owner of its delete record lives as long as the
# deleted owner: when the line leaves the undo.deleted ring, both go and the final state is freed.
# Covers the ring overflow, a ring change (resync_rings), undo inside the ring, and a store with the
# leaked undo owners of 0.20.0 (gc check and repair).
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
# Undo owners whose line is deleted and whose delete record is outside the deleted ring.
stale_undo() {
    q "select count(*) from owners o join lines l on l.id=substr(o.holder,1,instr(o.holder,'|')-1)
       where o.kind='undo' and l.deleted=1 and not exists (select 1 from owners d where d.kind='deleted'
       and d.holder=substr(o.holder,instr(o.holder,'|')+1)||'|'||l.id)"
}
# Every owner that names a deleted line (undo or deleted), by the line id in the holder.
owners_of_line() { q "select count(*) from owners where holder like '$1|%' or holder like '%|$1'"; }
# The final state of a line: its newest automatic state (the working state of its delete record).
newest_auto() { q "select id from states where kind='auto' and line='$(line_id "$1")' order by time desc, id desc limit 1"; }
# The ord of the newest save record of the project (the holder form of an undo owner).
newest_save_ord() { q "select printf('%016d:%s:%012d', time, machine, seq) from records where op='save' order by time desc, seq desc limit 1"; }

# 1. The first deleted line's final state is freed when the line leaves the undo.deleted ring.
newproj d
l config undo.deleted 2 >/dev/null
l line new -q d1 >/dev/null
cd $M/d1 || exit 1
for i in 1 2 3; do echo "d1 $i" > d1.txt; l save -q; done
d1id=$(line_id d1)
final=$(newest_auto d1)
ok "the first line has a final state with storage" on_disk "$final"
cd $M/main || exit 1
l line delete -D d1 >/dev/null
ok "the deleted line keeps its final state while it is in the ring" on_disk "$final"
for n in 2 3 4; do
    l line new -q d$n >/dev/null
    (cd $M/d$n && echo "d$n" > d$n.txt && l save -q) || exit 1
    cd $M/main || exit 1
    l line delete -D d$n >/dev/null
done
no "the final state of the first deleted line has no storage after it left the ring" on_disk "$final"
eq "no owner names the final state of the first deleted line" 0 "$(q "select count(*) from owners where state='$final'")"
eq "the first deleted line keeps no owner row" 0 "$(owners_of_line "$d1id")"
eq "gc check finds no stale undo owner" 0 "$(stale_undo)"
eq "gc check is clean after the ring overflow" 0 "$(l gc >/dev/null 2>&1; echo $?)"

# 2. A ring change (resync_rings) gives a deleted line no undo owner of its earlier operations.
newproj rs
l line new -q r >/dev/null
cd $M/r || exit 1
for i in 1 2 3 4; do echo "r $i" > r.txt; l save -q; done
rid=$(line_id r)
cd $M/main || exit 1
l line delete -D r >/dev/null
eq "the deleted line has no stale undo owner before the ring change" 0 "$(stale_undo)"
l config undo.ops 2 >/dev/null
eq "after undo.ops shrinks no undo owner is outside the deleted ring" 0 "$(stale_undo)"
eq "after undo.ops shrinks the deleted line keeps only the undo owner of its delete" 1 "$(q "select count(distinct holder) from owners where kind='undo' and holder like '$rid|%'")"
l config undo.ops 20 >/dev/null
eq "after undo.ops grows no undo owner is outside the deleted ring" 0 "$(stale_undo)"
eq "after undo.ops grows the deleted line keeps only the undo owner of its delete" 1 "$(q "select count(distinct holder) from owners where kind='undo' and holder like '$rid|%'")"

# 3. undo of a delete inside the ring brings the line back with its state.
newproj ud
l line new -q u >/dev/null
cd $M/u || exit 1
echo "u 1" > u.txt; l save -q; echo "u 2" > u.txt; l save -q
uid=$(line_id u)
uhead=$(q "select head from lines where id='$uid'")
ufinal=$(newest_auto u)
cd $M/main || exit 1
l line delete -D u >/dev/null
no "the deleted line is gone from the line list" sh -c "$LAYR line list | awk '{print \$1}' | grep -qx u"
l undo >/dev/null 2>&1
ok "undo of the delete restores the line" sh -c "$LAYR line list | awk '{print \$1}' | grep -qx u"
ok "undo of the delete restores the working file" test -f "$M/u/u.txt"
eq "undo of the delete restores the line head" "$uhead" "$(q "select head from lines where id='$uid'")"
ok "undo of the delete keeps the final state on disk" on_disk "$ufinal"

# 4. A store with the leaked undo owners of 0.20.0: gc check reports them, gc --repair frees them.
newproj lk
l config undo.deleted 1 >/dev/null
l line new -q g >/dev/null
cd $M/g || exit 1
for i in 1 2; do echo "g $i" > g.txt; l save -q; done
gid=$(line_id g)
gfinal=$(newest_auto g)
cd $M/main || exit 1
l line delete -D g >/dev/null
# The leftover of 0.20.0: the undo owner of an earlier record of g names its final state. The
# record is named by its real ord, so the holder has the form the store writes.
gord=$(newest_save_ord)
exec_sql "insert into owners(state, kind, holder) values('$gfinal', 'undo', '$gid|$gord')"
ok "the leaked undo owner keeps the final state while the line is in the ring" on_disk "$gfinal"
# Push g out of the undo.deleted ring (size 1).
l line new -q h >/dev/null
l line delete -D h >/dev/null
eq "one stale undo owner is outside the deleted ring" 1 "$(stale_undo)"
ok "the leaked owner keeps the final state on disk" on_disk "$gfinal"
has "gc check reports the stale undo owners" "stale undo" "$(l gc 2>&1 || true)"
no "gc check exits 1 with stale undo owners" l gc
l gc --repair >/dev/null 2>&1
no "gc --repair frees the final state of the deleted line" on_disk "$gfinal"
eq "gc --repair removes the stale undo owner" 0 "$(stale_undo)"
eq "gc check exits 0 after the repair" 0 "$(l gc >/dev/null 2>&1; echo $?)"
eq "fsck finds no problem after the repair" "0 problem(s)" "$(l fsck | tail -1)"

finish
