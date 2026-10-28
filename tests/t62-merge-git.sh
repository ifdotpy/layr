#!/bin/bash
# Merge in a git-backed project: files changed only on the fetched main are taken, a two-sided
# conflict is listed, a merge never carries open conflict markers on, and a save refuses
# conflict markers unless asked.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; for i in 1 2 3 4 5; do seq 1 10 | sed "s/^/f$i /" > src/f$i.txt; done
git add -A; git commit -qm "base"; git push -q origin main

l git clone $R gm -q >/dev/null
P=$LAYR_ROOT/projects/gm/lines
cd $P/main && l line new -q work
cd $P/work && echo mine > mine.txt && l add mine.txt && l save -qm "line work"

cd $U
for i in 1 2 3; do sed_inplace "s/^f$i 3\$/f$i 3 MAIN/" src/f$i.txt; done
git commit -qam "main 2"; git push -q origin main
cd $P/work && l git fetch -q
l merge -q origin/main >/dev/null 2>&1; rc=$?
eq "merge of origin/main exits 0" "0" "$rc"
for i in 1 2 3; do ok "incoming-only change f$i taken" grep -q "^f$i 3 MAIN" src/f$i.txt; done
ok "line work kept" test -f mine.txt

# Both sides change the same line: the conflict is listed.
cd $U; sed_inplace 's/^f4 5$/f4 5 MAIN/' src/f4.txt; git commit -qam "main 3"; git push -q origin main
cd $P/work; sed_inplace 's/^f4 5$/f4 5 LINE/' src/f4.txt; l save -qam "line f4"
l git fetch -q
out=$(l merge origin/main 2>&1); rc=$?
eq "conflicting merge exits 1" "1" "$rc"
has "conflict output names the path" "src/f4.txt" "$out"
eq "status shows the unmerged path" "UU src/f4.txt" "$(l status -s | grep f4)"

# A later change on main: a merge on top of open markers is refused and changes nothing.
cd $U; sed_inplace 's/^f5 7$/f5 7 MAIN/' src/f5.txt; git commit -qam "main 4"; git push -q origin main
cd $P/work; l git fetch -q
head=$(l rev-parse head)
out=$(l merge origin/main 2>&1); rc=$?
eq "merge over open conflicts is refused" "1" "$rc"
has "the refusal names the path" "src/f4.txt" "$out"
eq "the refused merge keeps the head" "$head" "$(l rev-parse head)"
no "the file of the later change is not touched" grep -q '^f5 7 MAIN' src/f5.txt

# A save refuses a file with a full set of markers unless the flag is given.
printf 'a\n<<<<<<< x\nb\n=======\nc\n>>>>>>> y\n' > marked.txt
l add marked.txt
out=$(l save -m "with markers" 2>&1); rc=$?
eq "save with markers refused" "1" "$rc"
has "the save refusal names the file" "marked.txt" "$out"
eq "a refused save keeps the head" "$head" "$(l rev-parse head)"
printf '<<<<<<< x\nonly one\n' > marked.txt
l add marked.txt
eq "an incomplete marker set is saved" "0" "$(l save -qm partial >/dev/null 2>&1; echo $?)"
printf 'a\n<<<<<<< x\nb\n=======\nc\n>>>>>>> y\n' > marked.txt
l add marked.txt
eq "the flag allows markers" "0" "$(l save -qm flagged --allow-conflict-markers >/dev/null 2>&1; echo $?)"

# Resolve, then the merge takes the incoming file.
for f in src/f4.txt marked.txt; do sed_inplace -E '/^<<<<<<<|^=======|^>>>>>>>/d' $f; done
l add src/f4.txt marked.txt && l save -qm resolved
eq "resolved: clean" "" "$(l status -s)"
l merge -q origin/main >/dev/null 2>&1
ok "after the resolution the incoming change is taken" grep -q '^f5 7 MAIN' src/f5.txt
finish
