#!/bin/bash
# ci: macos overlayfs
# A fetch stores every branch of the remote. A branch that it did not import is imported on first
# use (feedback item 5). After a fetch, git in a line sees the remote refs that layr has, not the
# ones of the line's start (feedback items 60 and 63).
. "$(dirname "$0")/lib.sh"

git init -q --bare $T/r.git
git clone -q $T/r.git $T/w 2>/dev/null
cd $T/w || exit 1
echo a > a && git add a && git commit -qm a && git push -q origin HEAD:main 2>/dev/null
git checkout -qb feat/x && echo x > x && git add x && git commit -qm x && git push -q origin feat/x 2>/dev/null

l git clone file://$T/r.git p -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main || exit 1
eq "a branch of the clone resolves" "$(git -C $T/r.git rev-parse feat/x)" "$(l rev-parse --git origin/feat/x)"

cd $T/w || exit 1
git checkout -qb feat/y && echo y > y && git add y && git commit -qm y && git push -q origin feat/y 2>/dev/null
git checkout -q main && echo a2 >> a && git commit -qam a2 && git push -q origin main 2>/dev/null
cd $LAYR_ROOT/projects/p/lines/main || exit 1
out=$(l git fetch origin 2>&1)
has "the fetch names the branch with its slash" "feat/x" "$out"
eq "a branch that the fetch did not import resolves" "$(git -C $T/r.git rev-parse feat/y)" "$(l rev-parse --git origin/feat/y)"
ok "a line starts from it" l line new -q fy origin/feat/y
eq "the line has the file of the branch" y "$(cat $LAYR_ROOT/projects/p/lines/fy/y)"
eq "git in the line sees the fetched main" "$(git -C $T/r.git rev-parse main)" "$(git rev-parse origin/main)"
eq "git in another line too" "$(git -C $T/r.git rev-parse main)" "$(git -C $LAYR_ROOT/projects/p/lines/fy rev-parse origin/main)"
no "an unknown branch stays unknown" l rev-parse origin/no-such
finish
