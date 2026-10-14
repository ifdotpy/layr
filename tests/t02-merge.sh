#!/bin/bash
# The merge engine through `layr merge`: tree cases, text merge, conflicts, drivers, block
# merge, fast-forward, squash, --expect, cherry-pick, revert, rebase.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src && cd $T/src
seq 1 10 | sed 's/^/a /' > A.txt; mkdir X Y; echo old > X/old.txt; echo a > Y/a; echo b > Y/b
ln -s t1 L; ln -s t1 M; ln -s t1 N; printf 'echo hi\n' > S.sh; chmod 644 S.sh; echo f > F.txt
echo q > Q.txt; echo c > c.txt; seq 1 10 | sed 's/^/z /' > Z.txt; echo w > W.txt
l init m --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/m/lines
cd $P/main && l branch -q topic
# ours (main)
cd $P/main
sed -i 's/^a 2$/a 2 OURS/' A.txt; rm -rf X; rm L; ln -s t2 L; rm N; ln -s t2 N; printf 'echo hi\necho ours\n' > S.sh
echo f-ours > F.txt; echo same > Same.txt; echo ours > Diff.txt; mv Q.txt Qo.txt; sed -i 's/^z 9$/z 9 OURS/' Z.txt; rm W.txt
l add -A && l commit -qm ours
# theirs (topic)
cd $P/topic
mv A.txt A2.txt; mkdir -p X; echo new > X/new.txt; rm M; ln -s t3 M; rm N; ln -s t3 N; chmod 755 S.sh
rm F.txt; mkdir F.txt; echo inner > F.txt/inner; echo same > Same.txt; echo theirs > Diff.txt; mv Q.txt Qt.txt
mv c.txt C.txt; rm -rf Y; sed -i 's/^z 1$/z 1 THEIRS/' Z.txt; echo w-theirs > W.txt
l add -A && l commit -qm theirs
cd $P/main
out=$(l merge topic 2>&1); rc=$?
eq "merge with conflicts exits 1" "1" "$rc"
C=$(l show -s --format=%B HEAD; l status)
ok "1 rename (theirs) vs modify (ours): A2.txt has the ours edit" grep -q 'a 2 OURS' A2.txt
no "1 A.txt is gone" test -e A.txt
has "2 folder deleted (ours) vs new file (theirs): conflict" "X/new.txt" "$C"
ok "2 the new file is kept" test -f X/new.txt
eq "3a symlink changed by ours only: kept" "t2" "$(readlink L)"
eq "3b symlink changed by theirs only: taken" "t3" "$(readlink M)"
has "3c symlink changed by both: conflict" "N (symlink target)" "$C"
ok "4 mode (theirs) and content (ours): both kept" sh -c 'grep -q ours S.sh && [ "$(stat -c %a S.sh)" = 755 ]'
has "5 file to folder vs modify: conflict" "F.txt (type change" "$C"
no "6a both add the same file: no conflict" sh -c "echo '$C' | grep -q 'Same.txt'"
has "6b both add different files: conflict" "Diff.txt (add/add)" "$C"
has "7 rename/rename: conflict" "Q.txt (rename/rename" "$C"
ok "8 case-only rename: applied" sh -c 'test -f C.txt && ! test -e c.txt'
no "9 folder deleted by theirs: removed" test -e Y
ok "10 edits of different lines: merged" sh -c "grep -q 'z 1 THEIRS' Z.txt && grep -q 'z 9 OURS' Z.txt"
has "11 ours deleted, theirs modified: conflict" "W.txt" "$C"
eq "merge state has two parents" "2" "$(l show -s --format=%p HEAD | wc -w)"

# text conflict and resolution
l reset -q --hard HEAD~1
cd $P/topic && l reset -q --hard HEAD~1 && sed -i 's/^z 5$/z 5 topic/' Z.txt && l commit -qam w5
cd $P/main && sed -i 's/^z 5$/z 5 main/' Z.txt && l commit -qam m5
eq "text conflict exits 1" "1" "$(l merge -q topic >/dev/null 2>&1; echo $?)"
ok "conflict markers written" grep -q '^<<<<<<< HEAD' Z.txt
eq "status shows the unmerged path" "UU Z.txt" "$(l status -s | grep Z.txt)"
sed -i '/^<<<<<<<\|^=======\|^>>>>>>>/d; s/^z 5 topic$//' Z.txt; sed -i '/^$/d' Z.txt
l add Z.txt && l commit -qm resolved
eq "resolved: no conflicts left" "" "$(l status -s)"
eq "the resolution state lists no conflicts" "" "$(l show -s --format=%B HEAD | grep -i conflict)"

# fast-forward, already up to date, squash, --expect
cd $P/topic && l reset -q --hard main && echo ff > ff.txt && l add ff.txt && l commit -qm ff
cd $P/main
has "fast-forward" "Fast-forward" "$(l merge topic)"
eq "already up to date" "Already up to date." "$(l merge topic)"
cd $P/topic && echo s1 > s1.txt && l add s1.txt && l commit -qm s1 && echo s2 > s2.txt && l add s2.txt && l commit -qm s2
cd $P/main && echo m > mm.txt && l add mm.txt && l commit -qm mm
l merge -q --squash -m "task: s1 and s2" topic
eq "squash: one parent" "1" "$(l show -s --format=%p HEAD | wc -w)"
ok "squash brings the files" test -f s2.txt
W=$(cd $P/topic && l rev-parse HEAD)
cd $P/topic && echo late > late.txt && l add late.txt && l commit -qm late
cd $P/main
eq "--expect refuses a moved line" "1" "$(l merge topic --expect $W >/dev/null 2>&1; echo $?)"
l merge -q $W
no "merge of an exact state does not take later work" test -e late.txt

# local changes are protected
cd $P/topic && echo w >> notes2.txt && l add notes2.txt && l commit -qm n2
cd $P/main && echo local > notes2.txt
eq "untracked file in the way: refused" "1" "$(l merge topic >/dev/null 2>&1; echo $?)"
eq "the local file is untouched" "local" "$(cat notes2.txt)"
rm notes2.txt

# cherry-pick and revert
cd $P/topic && echo pick > pick.txt && l add pick.txt && l commit -qm "pick me"
cd $P/main
l cherry-pick -q topic >/dev/null
eq "cherry-pick" "pick me" "$(l log -1 --format=%s)"
ok "cherry-pick brings the file" test -f pick.txt
l revert HEAD >/dev/null
eq "revert" 'Revert "pick me"' "$(l log -1 --format=%s)"
no "revert removes the file" test -e pick.txt

# rebase
cd $P/topic && l reset -q --hard main~2 && echo r1 > r1.txt && l add r1.txt && l commit -qm r1 && echo r2 > r2.txt && l add r2.txt && l commit -qm r2
l rebase -q main >/dev/null
eq "rebase replays two states" "r2 r1" "$(l log -2 --format=%s | tr '\n' ' ' | sed 's/ $//')"
ok "rebased onto main" l merge-base --is-ancestor main HEAD

# drivers: union, ours, blocks with verify, custom command
cd $P/main
printf 'list.txt merge=union\nkeep.txt merge=ours\n*.img merge=blocks merge-verify=size\nnum.txt merge=sum\n' > .layrattributes
printf 'a\n' > list.txt; echo base > keep.txt; head -c 65536 /dev/zero > disk.img; echo 1 > num.txt
l add -A && l commit -qm "driver base"
l config merge.driver.sum 'echo $(( $(cat %A) + $(cat %B) - $(cat %O) )) > %A'
STAT=$(command -v stat)
l config merge.verify.size "test \$($STAT -c %s %A) = 65536"
l branch -q d2
printf 'a\nmain\n' > list.txt; echo main > keep.txt; printf 'MAIN' | dd of=disk.img bs=1 seek=0 conv=notrunc status=none; echo 3 > num.txt
l commit -qam "main side"
cd $P/d2
printf 'a\nd2\n' > list.txt; echo d2 > keep.txt; printf 'D2!!' | dd of=disk.img bs=1 seek=40960 conv=notrunc status=none; echo 5 > num.txt
l commit -qam "d2 side"
cd $P/main
eq "drivers merge without conflicts" "0" "$(l merge -q d2 >/dev/null 2>&1; echo $?)"
eq "union keeps both lines" "a main d2" "$(tr '\n' ' ' < list.txt | sed 's/ $//')"
eq "ours keeps ours" "main" "$(cat keep.txt)"
eq "blocks: ours range" "MAIN" "$(head -c 4 disk.img)"
eq "blocks: theirs range" "D2!!" "$(dd if=disk.img bs=1 skip=40960 count=4 status=none)"
eq "custom driver" "7" "$(cat num.txt)"
# overlapping blocks are a conflict and keep ours
l branch -q d3
printf 'MINE' | dd of=disk.img bs=1 seek=100 conv=notrunc status=none; l commit -qam m-blk
cd $P/d3 && printf 'THEM' | dd of=disk.img bs=1 seek=102 conv=notrunc status=none && l commit -qam d3-blk
cd $P/main
eq "overlapping blocks: conflict" "1" "$(l merge -q d3 >/dev/null 2>&1; echo $?)"
eq "overlapping blocks keep ours" "MINE" "$(dd if=disk.img bs=1 skip=100 count=4 status=none)"
has "binary conflict listed" "disk.img" "$(l status -s)"
l checkout --theirs disk.img
eq "checkout --theirs takes the other version" "THEM" "$(dd if=disk.img bs=1 skip=102 count=4 status=none)"
l add disk.img && l commit -qm "take theirs"

finish
