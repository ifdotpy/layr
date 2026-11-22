#!/bin/bash
# The git commands of T3 Code in a fast line with a cache folder.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; work=$(mktemp -d $HOME/gitview/tmp/t3.XXXX); p=t3-$$
src=$work/src; mkdir -p $src/d; echo a > $src/a; echo b > $src/d/b
printf 'target regenerable\n' > $src/.layrattributes; printf 'target/\n' > $src/.gitignore
( cd $src && git init -q -b main && git add -A && git commit -q -m first )
$L init $p --from-git $src -q >/dev/null 2>&1 || { echo init failed; exit 1; }
cd $src; $L protect main --remove >/dev/null; $L migrate --delete-old >/dev/null
$L line new f --fast -q || exit 1; w=$LAYR_ROOT/projects/$p/lines/f; cd $w
mkdir -p target/debug; echo bin > target/debug/x; $L save --overlays -q >/dev/null 2>&1
echo "target is: $(stat -c %F target)"
echo "== git status (want empty):"; git status --porcelain
echo "== git clean -n -fd (want nothing):"; git clean -n -fd
echo "== T3 checkpoint: temp index, add -A, write-tree, commit-tree, update-ref"
echo edit >> a; echo new > n
export GIT_INDEX_FILE=$work/idx; git read-tree HEAD && git add -A && tree=$(git write-tree); unset GIT_INDEX_FILE
c=$(git commit-tree $tree -p HEAD -m checkpoint) && git update-ref refs/t3code/cp1 $c && echo "checkpoint $c"
echo "tree has target? $(git ls-tree -r --name-only $c | grep -c '^target')"
echo "layr head after checkpoint: $($L log -n 1 --format=%s)"
echo "== T3 diff against the checkpoint"; git diff --name-status HEAD $c
echo "== T3 restore: restore -s, clean -fd"
echo more >> a; echo junk > junk
git restore -s refs/t3code/cp1 -- . && git clean -fd
echo "a: $(tail -1 a), junk: $([ -e junk ] && echo stays || echo gone), n: $(cat n 2>&1)"
echo "target after clean: $(ls target/debug 2>&1)"
echo "== git status vs layr status"; git status --porcelain; $L status --porcelain
echo "== branch, switch, commit, pull --ff-only"
git switch -q -c feat && git add -A && git commit -q -m "t3 commit" && sleep 1
echo "layr head: $($L log -n 1 --format=%s)"
git init -q --bare $work/remote.git; git remote add origin $work/remote.git && git push -q -u origin feat && echo "pushed: $(git --git-dir=$work/remote.git log -n1 --format=%s feat)"
git fetch -q origin && git pull -q --ff-only && echo "pull ok"
echo "== worktree add"; s=$(date +%s%N); git worktree add -q $work/wt -b wt; echo "worktree add $(( ($(date +%s%N)-s)/1000000 )) ms; files: $(ls $work/wt | wc -l)"
git worktree list --porcelain | grep -c ^worktree
cd $src; $L line delete -D --discard f -q >/dev/null 2>&1
