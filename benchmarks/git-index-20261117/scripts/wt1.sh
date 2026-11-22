#!/bin/bash
# Check 1: plain git on the kernel tree. Time of git worktree add, --no-checkout, remove.
set -u
export GIT_CONFIG_GLOBAL=/dev/null
N=$HOME/gitview/kernel-plain; T=$HOME/gitview/tmp/wt; mkdir -p $T
now() { date +%s%N; }
t() { local s=$(now); "$@" >/dev/null 2>$T/err; local rc=$?; echo "$(( ($(now)-s)/1000000 )) ms rc=$rc : $*"; [ $rc = 0 ] || head -3 $T/err; }
cd $N
for i in 1 2 3; do
  t git worktree add -q $T/p$i -b p$i
  ( cd $T/p$i && t git status --porcelain )
  t git worktree remove --force $T/p$i
  git branch -q -D p$i
done
t git worktree add -q --no-checkout $T/pn -b pn
t git worktree remove --force $T/pn; git branch -q -D pn
echo load $(cut -d' ' -f1 /proc/loadavg)
