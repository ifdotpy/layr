#!/bin/bash
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=ck4$$
now() { date +%s%N; }; ms() { echo $(( ($(now)-$1)/1000000 )); }
t() { local name=$1; shift; local s=$(now); "$@" >/dev/null 2>/tmp/ck4.err; local rc=$?; printf '%-40s %7d ms rc=%s\n' "$name" $(( ($(now)-s)/1000000 )) $rc; [ $rc = 0 ] || head -2 /tmp/ck4.err; }
wait_head() { for i in $(seq 400); do [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] && return; sleep 0.05; done; echo "head did not follow" >&2; }
FILES=$(cd ~/gitview/kernel-src && git ls-files '*.c' | awk 'NR%48==1' | head -1260)
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
echo "checkout.workers: $(git config checkout.workers)"
git status --porcelain >/dev/null
for f in $FILES; do printf '/* big */\n' >> $f; done; git commit -q -am big; wait_head; git branch -f big; git checkout -q -b base HEAD~1; wait_head
for i in 1; do t "git checkout big ($(echo $FILES | wc -w) files)" git checkout -q big; wait_head; t "git checkout base" git checkout -q base; wait_head; done
t "git checkout big" git checkout -q big; wait_head
s=$(now); LAYR_TRACE=1 $L undo 2>/tmp/ck4.trace >/dev/null; echo "layr undo: $(ms $s) ms"; sort -k2 -n -r /tmp/ck4.trace | grep -v "open project\|project \|dead temp" | head -14
echo "branch: $(git symbolic-ref --short HEAD), big at $(git rev-parse --short big), git status lines: $(git status --porcelain | wc -l), layr status lines: $($L status --porcelain | wc -l)"
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
