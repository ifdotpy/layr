#!/bin/bash
# ci: macos overlayfs
# A command that is killed while it writes the .git folder of a line (feedback item 62) leaves a folder that the
# next refresh for the same head writes whole: `.git/layr-index` names the commit of the index, and a refresh
# skips the folder only when HEAD and it both name the commit. Here a save is killed between the writes of HEAD
# and of the index, and a push refreshes the folder.
. "$(dirname "$0")/lib.sh"

git init -q --bare $T/r.git
git clone -q $T/r.git $T/w 2>/dev/null
cd $T/w || exit 1
echo a > a && git add a && git commit -qm a && git push -q origin HEAD:main 2>/dev/null

l git clone file://$T/r.git p -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q w main >/dev/null
cd $LAYR_ROOT/projects/p/lines/w || exit 1
echo b >> a
LAYR_TEST_SAVE_KILL=git-folder-half l save -a -qm b >/dev/null 2>&1
eq "the save is recorded before the kill" b "$(l log -n 1 2>/dev/null | sed -n 's/^    //p' | head -1)"
l git push -q origin w:refs/heads/w >/dev/null 2>&1
eq "git in the line sees the saved commit" "$(l rev-parse --git head)" "$(git rev-parse HEAD)"
eq "after the push git in the line shows no change" "" "$(git status --porcelain)"
finish
