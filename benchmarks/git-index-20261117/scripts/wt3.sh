#!/bin/bash
# Check 3: git worktree add from a fast line. A hook makes a fast line and shows it at the path.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; P=gvkernel; LINES=$LAYR_ROOT/projects/$P/lines
T=$HOME/gitview/tmp/wt; mkdir -p $T; export T LINES
now() { date +%s%N; }
ms() { echo $(( ($(now)-$1)/1000000 )); }
w=$LINES/wsrc; cd $w
g=$(readlink .git)
[ -e $g/hooks/reference-transaction.layr ] || mv $g/hooks/reference-transaction $g/hooks/reference-transaction.layr
cat > $g/hooks/reference-transaction <<'H'
#!/bin/bash
in=$(cat)
if [ "$1" = committed ] && b=$(printf '%s\n' "$in" | sed -En 's|^0{40} ref:refs/heads/([^ ]+) HEAD$|\1|p') && [ -n "$b" ]; then
  common=$(git rev-parse --path-format=absolute --git-common-dir)
  for d in $common/worktrees/*/; do
    grep -qs initializing $d/locked || continue
    d=${d%/}; p=$(dirname "$(cat $d/gitdir)")
    s=$(date +%s%N)
    ( unset $(env | sed -n 's/^\(GIT_[A-Z_]*\)=.*/\1/p' | grep -v -e GIT_CONFIG_GLOBAL -e GIT_AUTHOR -e GIT_COMMITTER); $LAYR line new wt-$b --fast -q ) >> $T/hook.log 2>&1 || { echo "hook: line new failed" >> $T/hook.log; exit 0; }
    echo "hook: line new $(( ($(date +%s%N)-s)/1000000 )) ms" >> $T/hook.log
    lg=$(readlink $LINES/wt-$b/.git)
    cp $lg/index $d/index
    cp $lg/layr-index $d/layr-index
    mv $lg $lg.own && ln -s $d $lg
    sudo mount --bind $LINES/wt-$b $p || echo "hook: bind failed" >> $T/hook.log
    stat -c '%i %Y %n' $p/Makefile $p/kernel/fork.c $p/MAINTAINERS > $T/before
    echo "hook: all $(( ($(date +%s%N)-s)/1000000 )) ms" >> $T/hook.log
  done
fi
printf '%s\n' "$in" | "$(dirname "$0")/reference-transaction.layr" "$@"
exit 0
H
chmod +x $g/hooks/reference-transaction
: > $T/hook.log
b=${1:-feat1}; p=$T/$b
s=$(now); git worktree add -q $p -b $b; echo "git worktree add: $(ms $s) ms rc=$?"
cat $T/hook.log
stat -c '%i %Y %n' $p/Makefile $p/kernel/fork.c $p/MAINTAINERS > $T/after
cmp -s $T/before $T/after && echo "files: same inodes (git did not write them)" || { echo "files: REWRITTEN"; diff $T/before $T/after; }
echo "--- in the worktree path"
cd $p
ls -ld .git; findmnt -n -o SOURCE,FSTYPE $p
for i in 1 2 3; do s=$(now); n=$(git status --porcelain | wc -l); echo "git status: $(ms $s) ms, $n lines"; done
git log --oneline -n 1; git symbolic-ref HEAD
echo "--- in the folder of the line"
cd $LINES/wt-$b
s=$(now); n=$($L status --porcelain 2>&1 | wc -l); echo "layr status: $(ms $s) ms, $n lines"
$L status --porcelain 2>&1 | head -3
echo "--- from the source line"
cd $w; git worktree list; git branch --format='%(refname:short)' | tr '\n' ' '; echo
$L line list 2>&1 | head
echo load $(cut -d' ' -f1 /proc/loadavg)
