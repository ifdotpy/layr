#!/bin/bash
# An imported state is built from its parent state and the commit's changes. Its tree equals
# `git ls-tree -r` of the commit for edits, mode changes, renames, deletes (also of whole
# folders), file/folder swaps, links, submodules and merges, across layer chains that a short
# `thin.chain` cuts into complete trees.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p a b/deep swap/inner
echo x > a/x; echo y > a/y; echo f1 > b/deep/f1; echo f2 > b/deep/f2; echo t > top.txt
echo i > swap/inner/i; echo flip > flip; printf '#!/bin/sh\n' > run.sh; chmod +x run.sh; ln -s a/x link
git add -A
git update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,module
git commit -qm c1; git push -q origin main

l git clone $R it -q >/dev/null
P=$LAYR_ROOT/projects/it/lines
(cd $P/main && l config thin.chain 3)

cd $U
echo x2 >> a/x; mkdir -p c; echo n > c/new; chmod +x top.txt; git add -A; git commit -qm c2
git branch side
git mv a/y a/z; git rm -rq b; git commit -qm "c3 rename, folder delete"
git rm -rq swap; echo now-a-file > swap; git rm -q flip; mkdir flip; echo sub > flip/sub
git rm -q link; echo not-a-link > link; git update-index --force-remove module
git add -A; git commit -qm "c4 swaps"
git checkout -q side; echo s > side.txt; echo more >> a/x; git rm -q run.sh; git add -A; git commit -qm side1
git checkout -q main; git merge -q --no-ff -m "merge side" side
echo after >> c/new; git commit -qam c6
git checkout -q -b side2 HEAD~2; echo s2 > a/s2; git add -A; git commit -qm side2; git checkout -q main
git merge -q --no-ff -m "merge side2" side2
for i in 1 2 3; do echo "e$i" >> c/new; git commit -qam "e$i"; done
git push -q origin --all

cd $P/main
l git fetch -q origin
eq "every commit is imported" "$(git -C $U rev-list --count main)" "$(l log --format=%s origin/main | wc -l | tr -d ' ')"
if [ "$(uname -s)" = Darwin ]; then
    ok "imported states are thin layers over their parents" test "$(ls $LAYR_ROOT/projects/it/deltas | wc -l)" -ge 8
fi

# The tree of a line made from an imported state, as `git ls-tree -r` lists it.
listing() { # listing <folder>: "mode oid path" of files and links, "160000 - path" of empty folders
    (cd "$1" && find . -path ./.git -prune -o \( -type f -o -type l -o -type d \) -print | sed 's|^\./||' | grep -v '^\.$' | sort | while read -r f; do
        if [ -L "$f" ]; then echo "120000 $(readlink "$f" | tr -d '\n' | git hash-object --stdin) $f"
        elif [ -f "$f" ]; then m=100644; [ -x "$f" ] && m=100755; echo "$m $(git hash-object "$f") $f"
        elif [ -z "$(ls -A "$f")" ]; then echo "160000 - $f"; fi
    done)
}
expected() {
    git -C $U ls-tree -r "$1" | sed 's/^\([0-9]*\) [a-z]* \([0-9a-f]*\)\t/\1 \2 /' | while read -r m o f; do
        if [ "$m" = 160000 ]; then echo "160000 - $f"; else echo "$m $o $f"; fi
    done | sort -k3
}
n=0
for c in $(git -C $U rev-list main side side2 | sort -u); do
    n=$((n + 1))
    (cd $P/main && l line new -q chk$n $c) || { echo "FAIL line for $c"; FAIL=$((FAIL + 1)); continue; }
    eq "tree of imported commit $(git -C $U log -1 --format=%s $c) equals git ls-tree" "$(expected $c)" "$(listing $P/chk$n | sort -k3)"
done

# A second fetch builds on states that earlier layers hold.
cd $U; git checkout -q main; echo late > late.txt; git add late.txt; git commit -qm late; git push -q origin main
cd $P/main && l git fetch -q origin
(cd $P/main && l line new -q chklate origin/main)
eq "tree after a second fetch equals git ls-tree" "$(expected main)" "$(listing $P/chklate | sort -k3)"
finish
