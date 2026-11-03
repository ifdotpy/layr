#!/bin/bash
# layr follow (docs/follow.md): a line keeps a remote branch. A pass fast-forwards main; unsaved
# work skips it; a conflicting remote change leaves main unchanged and the error shows in
# `layr follow`; the caches of a line that was squash-merged on the remote come to main; `--remove`
# stops it. On macOS the daemon runs the same pass as the user. One user, so it runs on macOS and
# on Linux (the daemon part on macOS only).
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; echo base > src/a.txt; echo other > src/b.txt
printf 'target/\n' > .gitignore; echo 'target regenerable' > .layrattributes
git add -A; git commit -qm base; git push -q origin main

l git clone $R gm -q >/dev/null
P=$LAYR_ROOT/projects/gm/lines
cd $P/main

# 1. Set the target: one pass runs now. The remote is main, so nothing changes.
has "layr follow sets the target and reports up to date" "origin/main: up to date" "$(l follow origin/main 2>&1)"
has "layr follow shows the target and the last pass" "Following origin/main on line main" "$(l follow 2>&1)"

# 2. A pass fast-forwards main to a new remote commit.
cd $U; echo one > f1.txt; git add f1.txt; git commit -qm "remote one"; git push -q origin main
cd $P/main
before=$(l rev-parse head)
ok "layr follow --run exits 0" l follow --run
ok "main fast-forwarded: the remote file is in main" test -f f1.txt
ok "main head moved" test "$before" != "$(l rev-parse head)"

# 3. Unsaved work: the pass is skipped and main keeps its head and its work.
cd $U; echo two > f2.txt; git add f2.txt; git commit -qm "remote two"; git push -q origin main
cd $P/main
echo dirty >> src/b.txt
before=$(l rev-parse head)
out=$(l follow --run 2>&1)
has "a line with unsaved work is skipped" "skipped: unsaved work" "$out"
eq "main head did not move with unsaved work" "$before" "$(l rev-parse head)"
no "the remote file did not come with the skipped pass" test -f f2.txt
echo other > src/b.txt
ok "the next pass takes the remote change once the work is clean" l follow --run
ok "main has the second remote file" test -f f2.txt

# 4. Caches of a squash-merged line. feat saves caches and pushes its branch; the remote squash-merges
#    the same change into main. The pass takes the caches of feat into main.
l line new feat main >/dev/null
cd $P/feat
echo feat > feat.txt; l add feat.txt; l save -m "feat work" -q
rm -rf target; mkdir -p target/debug; echo bin-feat > target/debug/x
l save --caches -q
l git push -q origin HEAD:refs/heads/feat >/dev/null 2>&1
cd $U; git pull -q origin main; echo feat > feat.txt; git add feat.txt; git commit -qm "squash feat"; git push -q origin main
cd $P/main
ok "the pass with a squash-merged source succeeds" l follow --run
ok "main has the file of the squash-merged line" test -f feat.txt
eq "main has the caches of the squash-merged line" bin-feat "$(cat target/debug/x 2>/dev/null)"
has "the cache folder is an overlay of main" "target" "$(l overlay list 2>&1)"

# 5. A conflicting remote change: main keeps its head, and the error shows in layr follow.
echo local > src/a.txt; l save -a -m "local a" -q
cd $U; echo remote > src/a.txt; git commit -qam "remote a"; git push -q origin main
cd $P/main
before=$(l rev-parse head)
out=$(l follow --run 2>&1); rc=$?
eq "a conflicting pass exits 1" "1" "$rc"
eq "main head did not move on a conflict" "$before" "$(l rev-parse head)"
eq "the working file keeps the local content" local "$(cat src/a.txt)"
has "layr follow shows the conflict error" "conflicts: src/a.txt" "$(l follow 2>&1)"
eq "status shows no merge in progress" "" "$(l status -s | grep UU)"

# 6. Remove the target: no pass runs, and a run without a target fails.
ok "layr follow --remove succeeds" l follow --remove
has "after --remove the line follows nothing" "Not following on line main" "$(l follow 2>&1)"
cd $U; echo three > f3.txt; git add f3.txt; git commit -qm "remote three"; git push -q origin main
cd $P/main
no "layr follow --run without a target fails" l follow --run
no "main did not take the remote file after --remove" test -f f3.txt

# 7. macOS: the daemon runs the pass as the user (socket in the test folder). A fresh clone, so no
#    conflict of step 5 is in its way.
if [ "$(uname -s)" = Darwin ]; then
    l git clone $R gm2 -q >/dev/null
    D=$LAYR_ROOT/projects/gm2/lines/main
    cd $D && l follow origin/main >/dev/null 2>&1
    cd $U; echo four > f4.txt; git add f4.txt; git commit -qm "remote four"; git push -q origin main
    cd $D
    "$LAYR" daemon --tick 2 --socket $T/d.sock > $T/daemon.log 2>&1 &
    dpid=$!
    for i in $(seq 1 60); do test -f f4.txt && break; sleep 0.5; done
    kill $dpid 2>/dev/null; wait $dpid 2>/dev/null
    ok "the daemon pass moved main" test -f f4.txt
fi

finish
