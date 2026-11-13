#!/bin/bash
# ci: macos overlayfs
# gc and saved work (docs/ownership.md): no rule keeps the build output of pushed work. An
# automatic state goes when its ring (retention.auto, undo.ops) drops it, pushed or not, and a
# deleted line frees its automatic states in the delete. An automatic state with unsaved work
# stays while the undo ring names it. `gc` is a check: it exits 0 on a clean store.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; echo 'target/' > .gitignore; git add -A; git commit -qm one; git push -q origin main
l git clone $R p -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
DB=$LAYR_ROOT/projects/p/meta/db/layr.sqlite
[ -f "$DB" ] || DB=$LAYR_ROOT/projects/p/meta/layr.sqlite
cd $M/main || exit 1
id() { printf %s "$1" | awk '{print $NF}'; }
short() { printf %s "$1" | cut -c1-7; }
# present <display id>: 1 when the state is present in the database.
present() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); r=c.execute("select present from states where display=?", (sys.argv[2],)).fetchone(); print(r[0] if r else "missing")' "$DB" "$1"; }

# A line whose work went out, with build output in its automatic states.
l line new -q w origin/main >/dev/null
cd $M/w || exit 1
echo two > a.txt; l save -qam two
l git push -q origin w
mkdir -p target; head -c 1048576 /dev/urandom > target/out
built=$(id "$(l save)")
head -c 1048576 /dev/urandom > target/out2
built2=$(id "$(l save)")

# A line whose named work was never pushed, with build output in its automatic states.
cd $M/main || exit 1
l line new -q np origin/main >/dev/null
cd $M/np || exit 1
echo local > a.txt; l save -qam "not pushed"
mkdir -p target; head -c 1048576 /dev/urandom > target/out
npbuilt=$(id "$(l save)")

# A line with unsaved work.
cd $M/main || exit 1
l line new -q u origin/main >/dev/null
cd $M/u || exit 1
echo unsaved > a.txt
unsaved=$(id "$(l save)")

cd $M/main || exit 1
eq "gc check is clean with pushed work and build output" 0 "$(l gc >/dev/null 2>&1; echo $?)"
eq "the newest automatic state of a live line stays" 1 "$(present "$built2")"
eq "an older automatic state with pushed work stays inside the rings" 1 "$(present "$built")"

l line delete -D --discard w u np -q >/dev/null
eq "a deleted line frees its older automatic state, pushed work or not" 0 "$(present "$built")"
eq "a deleted line keeps its final working state in the undo.deleted ring" 1 "$(present "$npbuilt")"
eq "an automatic state with unsaved work stays in the undo ring" 1 "$(present "$unsaved")"
l gc --repair -q
eq "fsck is clean after gc" 0 "$(l fsck >/dev/null 2>&1; echo $?)"
finish
