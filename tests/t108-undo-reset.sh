#!/bin/bash
# Undo refreshes the .git folder of a line with its restored head: after undo of a pick and then of
# a reset, the line's .git/HEAD is the git commit of the restored head, the files and the status
# are the restored ones, and a later merge does not refuse with the .git/HEAD hint. The undo of
# the reset is refused while the pick is later, and an undo of a save refreshes the folder too.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
for i in $(seq 1 40); do echo "base $i" > f$i.txt; done
echo "F base" > F.txt; echo a > a1.txt
git add -A; git commit -qm base; git push -q origin main
l git clone $R gm -q >/dev/null
P=$LAYR_ROOT/projects/gm/lines

# Head D: a merge of line ma (a1.txt) and line mb (f1..f20, F.txt).
cd $P/main; S0=$(l rev-parse head)
l line new -q ma $S0
cd $P/ma; echo ma >> a1.txt; l save -a -m ma -q; MA=$(l rev-parse head)
cd $P/main; l line new -q mb $S0
cd $P/mb; for i in $(seq 1 20); do echo "D $i" > f$i.txt; done
echo D-version > F.txt; l save -a -m mb -q; MB=$(l rev-parse head)
cd $P/main
ok "merge ma into main" l merge ma --expect $MA
ok "merge mb into main" l merge mb --expect $MB
D=$(l rev-parse head)
ok "push main, so D has a git commit" l git push origin main -q
DG=$(l rev-parse --git $D)

# Head C: a line from S0 that changes f21..f40 and F.txt. Pick P: a line from C that changes
# F.txt again and adds pick.txt.
l line new -q cl $S0
cd $P/cl; for i in $(seq 21 40); do echo "C $i" > f$i.txt; done
echo C-version > F.txt; l save -a -m cl -q; C=$(l rev-parse head)
l line new -q pk $C
cd $P/pk; echo pick > pick.txt; l add pick.txt; echo PICK-version > F.txt
l save -a -m pk -q; PK=$(l rev-parse head)

# The same check for main and for a line that is not main. $1 is the line name.
undo_pick_reset() {
    local name=$1 dir=$P/$1 reset pick ch merge_out rc ph
    cd $dir
    l reset --hard $C >/dev/null
    reset=$(l op log -n 1 | awk '{print $1}')
    l pick $PK >/dev/null
    pick=$(l op log -n 1 | awk '{print $1}')
    ph=$(l rev-parse head)
    no "$name: undo of the reset is refused while the pick is later" l undo $reset
    eq "$name: the refused undo keeps the pick head" "$ph" "$(l rev-parse head)"
    ok "$name: undo of the pick" l undo $pick
    ok "$name: undo of the reset" l undo $reset
    eq "$name: head is D" "$D" "$(l rev-parse head)"
    eq "$name: .git/HEAD is the git commit of D" "$DG" "$(cat .git/HEAD)"
    eq "$name: F.txt is the content of D" "D-version" "$(cat F.txt)"
    eq "$name: status is clean" "" "$(l status --porcelain)"
    # A merge into the line goes through without the .git/HEAD hint.
    l line new -q "c-$name" $D
    (cd $P/c-$name && echo c > c-$name.txt && l add c-$name.txt && l save -a -m c -q)
    ch=$(cd $P/c-$name && l rev-parse head)
    merge_out=$(l merge c-$name --expect $ch 2>&1); rc=$?
    eq "$name: merge after the undo exits 0" 0 "$rc"
    eq "$name: merge prints no .git/HEAD hint" 0 "$(printf '%s' "$merge_out" | grep -c 'To keep the files')"
}

undo_pick_reset main
l line new -q work $D
undo_pick_reset work

# Undo of a save refreshes .git too: it goes back to the head and .git/HEAD from before the save.
cd $P/work; before=$(l rev-parse head); before_git=$(cat .git/HEAD)
echo save >> a1.txt; l save -a -m save -q; SAVE=$(l op log -n 1 | awk '{print $1}')
ok "undo of the save" l undo $SAVE
eq "undo of the save: head is the head before the save" "$before" "$(l rev-parse head)"
eq "undo of the save: .git/HEAD is the one before the save" "$before_git" "$(cat .git/HEAD)"

finish
