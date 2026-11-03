#!/bin/bash
# Removing a Git remote deletes its tracking refs from the Git bridge (issue 136): no line lists
# or exports them, layr line list -a drops them (also after a reindex), and the remote can come back.
. "$(dirname "$0")/lib.sh"
set -e

S=$T/local.git; O=$T/origin.git; W=$T/work
git init -q --bare -b main $S
git init -q --bare -b main $O
git clone -q $S $W 2>/dev/null; cd $W
echo one > a.txt; git add a.txt; git commit -qm "local 1"
git checkout -q -b feature; echo two > b.txt; git add b.txt; git commit -qm "local feature"
git push -q $S main feature
git checkout -q main; git push -q $O main

l git clone $O gp -q >/dev/null
cd $LAYR_ROOT/projects/gp/lines/main
l git remote add local $S
l git fetch -q local
l line new before origin/main >/dev/null
cd $LAYR_ROOT/projects/gp/lines/before
has "a line lists the refs of the remote before removal" "remotes/local/feature" "$(git branch -a)"
has "layr line list -a lists the refs of the remote before removal" "local/main" "$(l line list -a)"

l git remote remove local
l git sync -q
no "the existing line drops the refs of the removed remote" sh -c 'git branch -a | grep -q "remotes/local/"'
no "layr line list -a drops the refs of the removed remote" sh -c "$LAYR line list -a | grep -q 'local/'"

l line new after origin/main >/dev/null
cd $LAYR_ROOT/projects/gp/lines/after
no "a new line lists no refs of the removed remote" sh -c 'git branch -a | grep -q "remotes/local/"'
l reindex >/dev/null
no "the removal survives a reindex" sh -c "$LAYR line list -a | grep -q 'local/'"
eq "fetch from origin still works" "local 1" "$(l git fetch -q origin && l log -1 --format=%s origin/main)"

l git remote add local $S
l git fetch -q local
has "the remote can be added again and its refs return" "local/main" "$(l line list -a)"
finish
