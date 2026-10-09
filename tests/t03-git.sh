#!/bin/bash
# The remote bridge: clone, fetch, pull, push (one commit per task), a repeated push, a lost
# response after the push, the .git layer for tools, a rejected push.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; printf 'one\ntwo\nthree\n' > src/a.txt; echo '*.o' > .gitignore
git add -A; git commit -qm "upstream 1"; git tag v0.1; git push -q origin main --tags

l clone $R gp -q >/dev/null
cd $LAYR_ROOT/projects/gp/lines/main || exit 1
eq "clone: history from git" "upstream 1" "$(l log --format=%s)"
eq "clone: clean status" "" "$(l status -s)"
eq "git layer: clean for git" "" "$(git status --short)"
eq "git layer: describe works" "v0.1" "$(git describe --tags)"
eq "git layer: not part of states" "" "$(l show --name-only --format= HEAD | grep '^\.git/')"

cd $U; echo four >> src/a.txt; git commit -qam "upstream 2"; git push -q
cd $LAYR_ROOT/projects/gp/lines/main
l fetch -q
eq "fetch imports the branch" "upstream 2" "$(l log -1 --format=%s origin/main)"
echo local > local.txt; l add local.txt; l commit -qm "local work"
l pull -q >/dev/null
eq "pull merges" "four" "$(tail -1 src/a.txt)"
ok "pull keeps local work" test -f local.txt

out=$(l push origin task-1)
has "push makes a new branch" "[new branch]" "$out"
eq "one commit on top of upstream" "upstream 2" "$(git -C $R log -1 --format=%s task-1~1)"
eq "the commit has the task content" "local.txt" "$(git -C $R diff --name-only task-1~1 task-1)"
has "repeated push is a no-op" "Everything up-to-date" "$(l push origin task-1)"
c1=$(git -C $R rev-parse task-1)
git -C $R update-ref -d refs/heads/task-1
l push -q origin task-1
eq "the same state gives the same commit" "$c1" "$(git -C $R rev-parse task-1)"
eq "git layer after push: clean" "" "$(git status --short)"
eq "git layer HEAD is the pushed commit" "$c1" "$(git rev-parse HEAD)"

# Lost response: the push reached the remote, the record of it did not.
echo y >> local.txt; l commit -qam "second"
l push -n origin task-2 >/dev/null
sid=$(l rev-parse HEAD)
c2=$(git -C $LAYR_ROOT/projects/gp/git rev-parse refs/layr/states/$sid)
git -C $LAYR_ROOT/projects/gp/git push -q $R $c2:refs/heads/task-2
has "a repeated push reads the remote ref first" "Already pushed before" "$(l push origin task-2)"
eq "no second commit" "$c2" "$(git -C $R rev-parse task-2)"

# Someone else moved the branch: the push is refused with a hint.
cd $U; git fetch -q; git checkout -q -b task-2 origin/task-2; echo other > o.txt; git add o.txt; git commit -qm other; git push -q origin task-2
cd $LAYR_ROOT/projects/gp/lines/main
echo z >> local.txt; l commit -qam third
has "non-fast-forward push is refused" "layr pull" "$(l push origin task-2 2>&1)"

# A branch made by someone else, merged here.
cd $U; git checkout -q -b feature origin/main; echo feat > f.txt; git add f.txt; git commit -qm "feature work"; git push -q origin feature
cd $LAYR_ROOT/projects/gp/lines/main
l fetch -q origin feature
l merge -q origin/feature
ok "merge of an imported branch" test -f f.txt
eq "remote -v" "origin	$R (fetch)" "$(l remote -v | head -1)"
eq "open conflicts block a push" "1" "$(cd $U && git checkout -q main && echo X > src/a.txt && git commit -qam x && git push -q; cd $LAYR_ROOT/projects/gp/lines/main && echo Y > src/a.txt && l commit -qam y && l pull -q >/dev/null 2>&1; l push origin task-9 >/dev/null 2>&1; echo $?)"
finish
