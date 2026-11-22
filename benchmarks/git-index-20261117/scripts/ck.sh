#!/bin/bash
# Item 3: switch between two commits that differ in N files: plain git, git in a fast line,
# layr reset --hard, and the view's goto (the tree changes, no file is written).
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=ck$$; N=${1:-2000}; P=$HOME/gitview/kernel-plain
now() { date +%s%N; }
t() { local name=$1; shift; local s=$(now); "$@" >/dev/null 2>/tmp/ck.err; local rc=$?; printf '%-34s %7d ms rc=%s\n' "$name" $(( ($(now)-s)/1000000 )) $rc; [ $rc = 0 ] || head -2 /tmp/ck.err; }
wait_head() { for i in $(seq 200); do [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] && return; sleep 0.05; done; echo "head did not follow" >&2; }
FILES=$(cd ~/gitview/kernel-src && git ls-files '*.c' | awk -v n=$N 'NR%int(60000/n)==1' | head -$N)
echo "files changed between the two commits: $(echo $FILES | wc -w)"
# plain git
cd $P; git checkout -q -B base; for f in $FILES; do printf '/* big */\n' >> $f; done; git commit -q -am big; git branch -f big; git reset -q --hard HEAD~1
for i in 1 2; do t "plain: git checkout big" git checkout -q big; t "plain: git checkout base" git checkout -q base; done
git branch -q -D big
# fast line
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
git status --porcelain >/dev/null
base=$($L rev-parse head)
for f in $FILES; do printf '/* big */\n' >> $f; done; git commit -q -am big; wait_head
big=$($L rev-parse head); git branch -f big; git checkout -q -b base HEAD~1; wait_head
for i in 1 2; do
  t "fast: git checkout big" git checkout -q big; wait_head
  t "fast: git checkout base" git checkout -q base; wait_head
done
echo "git status lines: $(git status --porcelain | wc -l)"
for i in 1 2; do
  t "fast: layr reset --hard big" $L reset -q --hard $big
  t "fast: layr reset --hard base" $L reset -q --hard $base
done
sock=$(findmnt -n -o SOURCE $LINES/$n | sed 's/^layrfs-view://')
for i in 1 2; do
  t "fast: view goto big (tree only)" sudo $L fs view ctl $sock goto $big
  t "fast: view goto base (tree only)" sudo $L fs view ctl $sock goto $base
done
sudo $L fs view ctl $sock goto $base 2>&1 | tail -1
echo "layr status lines after goto back: $($L status --porcelain | wc -l)"
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
