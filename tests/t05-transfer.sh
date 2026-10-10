#!/bin/bash
# Copies outside the store: export (with the APFS name rules), backups of the whole project
# and their restore on another file system.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init x --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/x/lines
cd $P/main
git init -q --bare $T/r.git; l remote add origin $T/r.git

# export
l export $T/exp -q
eq "export writes the working content" "$(cat notes.txt)" "$(cat $T/exp/notes.txt)"
echo e >> notes.txt
eq "incremental export writes one path" "exported" "$(l export $T/exp | awk '{print $1}')"
has "incremental export count" "1 path(s)" "$(echo e2 >> notes.txt; l export $T/exp)"
echo secret > local.log
l export -q $T/exp
no "export leaves ignored files out" test -e $T/exp/local.log
echo secret2 > local2.log; echo t >> notes.txt
l export -q $T/exp
no "an incremental export leaves ignored files out too" test -e $T/exp/local2.log
l export -q --all $T/expall
ok "export --all writes ignored files" test -f $T/expall/local.log
rm local.log local2.log
mkdir -p $T/busy; echo keep > $T/busy/x
eq "export refuses a foreign folder" "1" "$(l export $T/busy >/dev/null 2>&1; echo $?)"
echo A > Readme; echo B > README
eq "export reports names that collide on APFS" "1" "$(l export $T/exp2 >/dev/null 2>&1; echo $?)"
has "the colliding names are listed" "README" "$(l export $T/exp3 2>&1)"
no "colliding names are not written" test -e $T/exp3/README
rm Readme README

echo q >> notes.txt

# backup and restore of the whole project
l commit -qam "before backup"; l branch -q side; l tag v1
cd $P/side && echo side > side.txt && l add side.txt && l commit -qm side && cd $P/main
l push -q origin task-1
echo wip > wip.txt
l backup -q $T/bk
echo more >> notes.txt; l commit -qam "after first backup"
l backup -q $T/bk
eq "backup verify" "ok" "$(l backup verify $T/bk | awk '{print $NF}')"
new_fs $T/b 6G
LAYR_ROOT=$T/b/store l backup restore -q $T/bk
cd $T/b/store/projects/x/lines/main || exit 1
B="env LAYR_ROOT=$T/b/store"
eq "restore: same history" "$(cd $P/main && l log --format='%H %s')" "$($B $LAYR "log" --format='%H %s')"
eq "restore: lines" "main side" "$($B $LAYR branch | tr -d '* ' | tr '\n' ' ' | sed 's/ $//')"
eq "restore: side line content" "side" "$(cat $T/b/store/projects/x/lines/side/side.txt)"
eq "restore: uncommitted work of the line" "wip" "$(cat wip.txt)"
eq "restore: tags" "v1" "$($B $LAYR tag)"
has "restore: push records (the export table)" "task-1" "$($B $LAYR op log --all -n 200 --json | grep -o 'task-1' | head -1)"
eq "restore: the remote push is idempotent" "0" "$($B $LAYR push -q origin task-1 >/dev/null 2>&1; echo $?)"
$B $LAYR undo >/dev/null 2>&1
eq "restore: undo works on restored history" "before backup" "$($B $LAYR log -1 --format=%s)"
eq "restore: fsck" "0 problem(s)" "$($B $LAYR fsck | tail -1)"
f=$(ls $T/bk/*/chain-0001/*stream.zst | head -1); printf 'XX' | dd of=$f bs=1 seek=100 conv=notrunc status=none
eq "a damaged stream fails verify" "1" "$(l backup verify $T/bk >/dev/null 2>&1; echo $?)"
finish
