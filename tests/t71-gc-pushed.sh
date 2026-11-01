#!/bin/bash
# gc and saved work: the undo window and the automatic-state rules do not keep an automatic
# state whose parent a push holds and whose files that are not ignored equal it (only build
# output differs). An automatic state with unsaved work stays for the undo window.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; echo 'target/' > .gitignore; git add -A; git commit -qm one; git push -q origin main
l git clone $R p -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd $M/main || exit 1
id() { printf %s "$1" | awk '{print $NF}'; }
short() { printf %s "$1" | cut -c1-7; }

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
plan=$(l gc -n)
has "an older automatic state with pushed work and build output goes" "$(short $built)" "$plan"
no "the newest automatic state of a live line stays" sh -c "printf %s \"\$1\" | grep -q $(short $built2)" _ "$plan"
l line delete -D w u np -q >/dev/null
plan=$(l gc -n)
has "after the line is deleted, its last automatic state goes too" "$(short $built2)" "$plan"
has "a deleted line's build-output state goes even when its work was never pushed" "$(short $npbuilt)" "$plan"
no "an automatic state with unsaved work stays in the undo window" sh -c "printf %s \"\$1\" | grep -q $(short $unsaved)" _ "$plan"
l gc -q
eq "fsck is clean after gc" 0 "$(l fsck >/dev/null 2>&1; echo $?)"
finish
