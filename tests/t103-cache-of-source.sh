#!/bin/bash
# ci: overlayfs macos-plain
# A new line from a state takes the cache saved with that state (`cache_of`). A state imported
# from git has no line: the old rule walked to the nearest ancestor that a line made (here a line
# that pushed its commit) and took that line's old cache.
. "$(dirname "$0")/lib.sh"

# A Git remote with one commit; the project starts from it.
sample $T/git-src
echo 'target regenerable' > $T/git-src/.layrattributes
git init -q -b main $T/git-src
git -C $T/git-src add -A && git -C $T/git-src commit -qm one
git clone -q --bare $T/git-src $T/origin.git
git clone -q $T/origin.git $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l git remote -v | grep -q '^origin' || l git remote add origin $T/origin.git >/dev/null

# A line makes a commit, saves an old cache and pushes the commit to the remote main.
l line new old main >/dev/null
cd $P/old
echo change >> notes.txt
l add notes.txt && l save -qm "old work" >/dev/null
rm -rf target; mkdir -p target/debug; echo old > target/debug/x
l save --glasses -q
l git push origin HEAD:refs/heads/main >/dev/null 2>&1
ok "the old line's commit is on the remote" git -C $T/origin.git rev-parse --verify -q main

# Someone adds a commit on top in Git. Main follows the remote, so its head is a state
# imported from git whose parent is the old line's state. `save --glasses` saves with an
# automatic state over that head.
git clone -q $T/origin.git $T/work2
echo more >> $T/work2/README.md
git -C $T/work2 add README.md && git -C $T/work2 commit -qm two && git -C $T/work2 push -q origin main
cd $P/main
l git fetch origin >/dev/null 2>&1
head=$(l rev-parse origin/main)
l merge origin/main -q >/dev/null 2>&1 || l merge origin/main --expect "$head" -q >/dev/null
eq "main is at the imported state" "$head" "$(l rev-parse main)"
rm -rf target; mkdir -p target/debug; echo new > target/debug/x
l save --glasses -q
eq "main stays at the imported state" "$head" "$(l rev-parse main)"

# A line from that state takes main's new cache, not the old line's.
l line new from-state "$head" >/dev/null
eq "a line from the imported state takes the cache saved with it" new "$(cat $P/from-state/target/debug/x 2>&1)"
l line new from-ref origin/main >/dev/null
eq "a line from origin/main takes the same cache" new "$(cat $P/from-ref/target/debug/x 2>&1)"
l line new from-line main >/dev/null
eq "a line from the line main takes main's cache" new "$(cat $P/from-line/target/debug/x 2>&1)"

finish
