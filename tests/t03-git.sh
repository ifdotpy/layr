#!/bin/bash
# The remote bridge: clone, fetch, pull, push (one commit per task), a repeated push, a lost
# response after the push, the .git folder for tools, a rejected push.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; printf 'one\ntwo\nthree\n' > src/a.txt; echo '*.o' > .gitignore
git add -A; git commit -qm "upstream 1"; git tag v0.1; git push -q origin main --tags

l git clone $R gp -q >/dev/null
cd $LAYR_ROOT/projects/gp/lines/main || exit 1
eq "clone: history from git" "upstream 1" "$(l log --format=%s)"
eq "clone: clean status" "" "$(l status -s)"
eq "git folder: clean for git" "" "$(git status --short)"
eq "git folder: describe works" "v0.1" "$(git describe --tags)"
eq "git folder: not part of states" "" "$(l show --name-only --format= head | grep '^\.git/')"

cd $U; echo four >> src/a.txt; git commit -qam "upstream 2"; git push -q
cd $LAYR_ROOT/projects/gp/lines/main
l git fetch -q
eq "fetch imports the branch" "upstream 2" "$(l log -1 --format=%s origin/main)"
echo local > local.txt; l add local.txt; l save -qm "local work"
l git pull -q >/dev/null
eq "pull merges" "four" "$(tail -1 src/a.txt)"
ok "pull keeps local work" test -f local.txt

out=$(l git push --squash origin task-1)
has "push makes a new branch" "[new branch]" "$out"
eq "one commit on top of upstream" "upstream 2" "$(git -C $R log -1 --format=%s task-1~1)"
eq "the commit has the task content" "local.txt" "$(git -C $R diff --name-only task-1~1 task-1)"
c1=$(git -C $R rev-parse task-1)
# A push without --squash sends the history of the state, never the squash commit made before.
has "a push without --squash does not reuse the squash commit" "layr git pull" "$(l git push origin task-1 2>&1)"
has "repeated push is a no-op" "Everything up-to-date" "$(l git push --squash origin task-1)"
hs=$(l rev-parse head); hc=$(git -C $LAYR_ROOT/projects/gp/git rev-parse "refs/layr/states/$hs" 2>/dev/null)
git -C $R update-ref -d refs/heads/task-1
l git push -q origin task-1
eq "a push without --squash sends the history commit of the state" "$hc" "$(git -C $R rev-parse task-1)"
ok "the history commit is not the squash commit" test "$c1" != "$hc"
has "the history keeps the line's commits" "local work" "$(git -C $R log --format=%s task-1)"
eq "git folder after push: clean" "" "$(git status --short)"
eq "git folder HEAD is the pushed commit" "$hc" "$(git rev-parse HEAD)"

# Lost response: the push reached the remote, the record of it did not.
echo y >> local.txt; l save -qam "second"
l git push -n origin task-2 >/dev/null
sid=$(l rev-parse head)
c2=$(git -C $LAYR_ROOT/projects/gp/git rev-parse refs/layr/states/$sid)
git -C $LAYR_ROOT/projects/gp/git push -q $R $c2:refs/heads/task-2
has "a repeated push reads the remote ref first" "Already pushed before" "$(l git push origin task-2)"
eq "no second commit" "$c2" "$(git -C $R rev-parse task-2)"

# Someone else moved the branch: the push is refused with a hint.
cd $U; git fetch -q; git checkout -q -b task-2 origin/task-2; echo other > o.txt; git add o.txt; git commit -qm other; git push -q origin task-2
cd $LAYR_ROOT/projects/gp/lines/main
echo z >> local.txt; l save -qam third
has "non-fast-forward push is refused" "layr git pull" "$(l git push origin task-2 2>&1)"

# git-sync: the .git folder shows the head without a remote; a later push sends the same commit.
echo view > view.txt; l add view.txt; l save -qm "local view"
refs_before=$(git -C $R for-each-ref)
l git sync -q
sid=$(l rev-parse head)
cv=$(git -C $LAYR_ROOT/projects/gp/git rev-parse refs/layr/states/$sid)
eq "git-sync: .git HEAD is the commit of the head state" "$cv" "$(git rev-parse HEAD)"
eq "git-sync: clean for git" "" "$(git status --short)"
eq "the tree hash of the head is the tree of its git commit" "$(git rev-parse 'HEAD^{tree}')" "$(l rev-parse 'head^{tree}')"
# <rev>:<path> is the object of that path in the exported commit: a tree for a folder, a blob
# for a file, the root tree for an empty path.
G_STORE=$LAYR_ROOT/projects/gp/git
eq "rev-parse <rev>:<dir> is the tree in the exported commit" "$(git -C $G_STORE rev-parse $cv:src)" "$(l rev-parse head:src)"
eq "rev-parse <rev>:<file> is the blob in the exported commit" "$(git -C $G_STORE rev-parse $cv:src/a.txt)" "$(l rev-parse head:src/a.txt)"
eq "rev-parse <rev>: is the root tree" "$(git -C $G_STORE rev-parse "$cv^{tree}")" "$(l rev-parse head:)"
eq "rev-parse <rev>: is the same as <rev>^{tree}" "$(l rev-parse 'head^{tree}')" "$(l rev-parse head:)"
eq "rev-parse --short <rev>:<file>" "$(git rev-parse --short HEAD:src/a.txt)" "$(l rev-parse --short head:src/a.txt)"
cd src
eq "rev-parse <rev>:./<file> resolves in the current folder" "$(git rev-parse HEAD:./a.txt)" "$(l rev-parse head:./a.txt)"
eq "rev-parse <rev>:../<dir> resolves in the current folder" "$(git rev-parse HEAD:../src)" "$(l rev-parse head:../src)"
cd ..
# git also echoes the argument on stdout; layr prints only the error, as for other bad revisions.
out=$(l rev-parse head:nope 2>&1 >/dev/null); rc=$?
eq "rev-parse <rev>:<missing> exits 128" 128 "$rc"
git_missing=$(git rev-parse HEAD:nope 2>&1 >/dev/null)
eq "rev-parse <rev>:<missing> fails like git" "${git_missing/HEAD/head}" "$out"
cd src
git_missing=$(git rev-parse HEAD:./nope 2>&1 >/dev/null)
eq "rev-parse <rev>:./<missing> names the full path like git" "${git_missing/HEAD/head}" "$(l rev-parse head:./nope 2>&1 >/dev/null)"
cd ..
l rev-parse -q head:nope >/dev/null 2>&1; rc=$?
eq "rev-parse -q <rev>:<missing> exits 1 quietly" 1 "$rc"
eq "git-sync: the remote is untouched" "$refs_before" "$(git -C $R for-each-ref)"
records=$(l op log -n 50 | wc -l)
l git sync -q
eq "git-sync again: no new record" "$records" "$(l op log -n 50 | wc -l)"
no "the git-sync remote name is reserved" l git remote add git-sync $R
l git push -q origin view-1
eq "a later push sends the same commit" "$cv" "$(git -C $R rev-parse view-1)"

# Every move of the head points .git HEAD at the commit of the new head: the commit that a push
# sends for it, with the previous head's commit as parent. Tools that read git HEAD see the head.
echo h1 > head.txt; l add head.txt; l save -qm "head 1"
h1=$(git rev-parse HEAD)
eq "commit: .git HEAD has the tree of the new head" "$(l rev-parse 'head^{tree}')" "$(git rev-parse 'HEAD^{tree}')"
eq "commit: its parent is the commit of the previous head" "$cv" "$(git rev-parse HEAD^)"
eq "commit: its subject is the message" "head 1" "$(git log -1 --format=%s)"
eq "commit: clean for git" "" "$(git status --short)"
has "commit: the commit is the newest record (no export record)" " state " "$(l op log --all -n 1)"
eq "layr reads the commit id of .git HEAD" "$(l rev-parse head)" "$(l rev-parse $h1)"
echo h2 >> head.txt; l save -qam "head 2"
eq "the next commit moves HEAD forward" "$h1" "$(git rev-parse HEAD^)"
l save -q --amend -m "head 2 amended"
eq "amend: HEAD is the amended state on the same parent" "$h1 head 2 amended" "$(git rev-parse HEAD^) $(git log -1 --format=%s)"
l reset -q --hard head~1
eq "reset: HEAD is the commit of the state it went to" "$h1" "$(git rev-parse HEAD)"
eq "reset: clean for git" "" "$(git status --short)"
echo h3 >> head.txt; l save -qam "head 3"
h3=$(git rev-parse HEAD)
l git push -q origin head-3
eq "a push sends the commit that .git HEAD shows" "$h3" "$(git -C $R rev-parse head-3)"
eq "and HEAD stays" "$h3" "$(git rev-parse HEAD)"
ok "the Git bridge accepts HEAD as a push source" l git push -q origin HEAD:head-alias
eq "the HEAD push publishes the current commit" "$h3" "$(git -C $R rev-parse head-alias)"

# try: a temporary copy of a state, with that state's commit in its .git folder.
l try -q head~1 -- sh -c 'git rev-parse HEAD; git status --porcelain; cat head.txt' > $T/try.out
eq "try: .git HEAD of the copy is the commit of the state, clean" "$h1 h1" "$(xargs < $T/try.out)"
eq "try: the line did not change" "$h3" "$(git rev-parse HEAD)"
has "try: a missing command is named" "cannot run 'no-such-command-x'" "$(l try -q head -- no-such-command-x 2>&1)"
has "try: a bad revision is named" "layr try: unknown revision 'nope'" "$(l try -q nope -- true 2>&1)"

# A branch made by someone else, merged here.
cd $U; git checkout -q -b feature origin/main; echo feat > f.txt; git add f.txt; git commit -qm "feature work"; git push -q origin feature
cd $LAYR_ROOT/projects/gp/lines/main
l git fetch -q origin feature
l merge -q origin/feature
ok "merge of an imported branch" test -f f.txt
eq "remote -v" "origin	$R (fetch)" "$(l git remote -v | head -1)"
eq "open conflicts block a push" "1" "$(cd $U && git checkout -q main && echo X > src/a.txt && git commit -qam x && git push -q; cd $LAYR_ROOT/projects/gp/lines/main && echo Y > src/a.txt && l save -qam y && l git pull -q >/dev/null 2>&1; l git push origin task-9 >/dev/null 2>&1; echo $?)"
# Git writes in a line's .git are refused: layr writes the line's history, and git HEAD must
# stay at the last commit layr exported. Nothing moves; a reset to that commit lets layr go on.
l git clone $R gx -q >/dev/null
cd $LAYR_ROOT/projects/gx/lines/main || exit 1
echo g1 > guard.txt; l add guard.txt; l save -qm "guard 1"
g1=$(git rev-parse HEAD); gh=$(l rev-parse head)
chmod -R u+w .git # the guard is read only; open it to simulate a git write
echo g2 > guard2.txt; git add guard2.txt; git commit -qm "git work"
gc=$(git rev-parse HEAD)
echo g3 > guard3.txt; l add guard3.txt
out=$(l save -qm "guard 2" 2>&1); rc=$?
eq "save refuses after a git commit in the line" "refused" "$([ "$rc" -ne 0 ] && echo refused)"
has "the refusal names the git HEAD" "A git commit, merge, reset or checkout ran in this line ($gc)" "$out"
eq "the refused save leaves git HEAD" "$gc" "$(git rev-parse HEAD)"
eq "the refused save leaves the line head" "$gh" "$(l rev-parse head)"
git reset -q --hard $g1
eq "after a reset to the last export, save builds on it" "$g1" "$(l save -qm "guard 4" >/dev/null 2>&1; git rev-parse HEAD^)"

# The same for a git merge in the line.
cd $LAYR_ROOT && l git clone $R gy -q >/dev/null
cd $LAYR_ROOT/projects/gy/lines/main || exit 1
echo y1 > guard.txt; l add guard.txt; l save -qm "guard 1"
y1=$(git rev-parse HEAD); yh=$(l rev-parse head)
chmod -R u+w .git
git checkout -q -b side; echo s > side.txt; git add side.txt; git commit -qm "side work"
git checkout -q --detach $y1; echo m > main.txt; git add main.txt; git commit -qm "main work"
git merge -q --no-edit side; ym=$(git rev-parse HEAD)
echo y2 > guard2.txt; l add guard2.txt
out=$(l save -qm "guard 2" 2>&1); rc=$?
eq "save refuses after a git merge in the line" "refused" "$([ "$rc" -ne 0 ] && echo refused)"
has "the merge refusal names the git HEAD" "A git commit, merge, reset or checkout ran in this line ($ym)" "$out"
eq "the refused save leaves the merge in git HEAD" "$ym" "$(git rev-parse HEAD)"
eq "the refused save leaves the line head of the merge case" "$yh" "$(l rev-parse head)"
finish
