#!/bin/bash
# Check 2: plain folders. A hook fills the new folder and its index while git worktree add runs.
# Does git write the files again?
set -u
export GIT_CONFIG_GLOBAL=/dev/null
N=$HOME/gitview/kernel-plain; T=$HOME/gitview/tmp/wt; mkdir -p $T
now() { date +%s%N; }
cd $N
git config core.checkStat minimal; git config core.trustctime false
git status --porcelain >/dev/null
cat > .git/hooks/reference-transaction <<'H'
#!/bin/bash
[ "$1" = committed ] || exit 0
grep -Eq '^0{40} ref:refs/heads/[^ ]+ HEAD$' || exit 0
T=$HOME/gitview/tmp/wt
common=$(git rev-parse --git-common-dir)
for d in $common/worktrees/*/; do
  grep -qs initializing $d/locked || continue
  p=$(dirname "$(cat $d/gitdir)")
  s=$(date +%s%N)
  echo "hook: worktree $d path $p has: $(ls -A $p | tr '\n' ' ')" >> $T/hook.log
  ( cd $(git rev-parse --show-toplevel) && find . -mindepth 1 -maxdepth 1 ! -name .git -exec cp -a --reflink=always -t $p {} + )
  cp $common/index $d/index
  stat -c '%i %Y %n' $p/Makefile $p/kernel/fork.c $p/MAINTAINERS > $T/before
  echo "hook: filled in $(( ($(date +%s%N)-s)/1000000 )) ms" >> $T/hook.log
done
exit 0
H
chmod +x .git/hooks/reference-transaction
: > $T/hook.log
for i in 1 2; do
  s=$(now); git worktree add -q $T/q$i -b q$i; rc=$?
  echo "git worktree add: $(( ($(now)-s)/1000000 )) ms rc=$rc"
  cat $T/hook.log; : > $T/hook.log
  stat -c '%i %Y %n' $T/q$i/Makefile $T/q$i/kernel/fork.c $T/q$i/MAINTAINERS > $T/after
  cmp -s $T/before $T/after && echo "files: same inodes (git did not write them)" || { echo "files: REWRITTEN"; diff $T/before $T/after; }
  ( cd $T/q$i && s=$(now) && git status --porcelain | head -3 && echo "git status: $(( ($(now)-s)/1000000 )) ms" && git log --oneline -n 1 && git symbolic-ref HEAD )
  git worktree list
  git worktree remove --force $T/q$i; git branch -q -D q$i
done
# The same without the index: git must write.
sed -i 's|^  cp \$common/index.*|  :|' .git/hooks/reference-transaction
s=$(now); git worktree add -q $T/q3 -b q3; echo "without the index: $(( ($(now)-s)/1000000 )) ms rc=$?"; cat $T/hook.log
stat -c '%i %Y %n' $T/q3/Makefile $T/q3/kernel/fork.c $T/q3/MAINTAINERS > $T/after
cmp -s $T/before $T/after && echo "files: same inodes" || echo "files: REWRITTEN"
git worktree remove --force $T/q3; git branch -q -D q3
rm .git/hooks/reference-transaction
echo load $(cut -d' ' -f1 /proc/loadavg)
