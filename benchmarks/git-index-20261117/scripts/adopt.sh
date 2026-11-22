#!/bin/bash
# Item 1: where the follow of a git commit spends its time.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=ad$$
now() { date +%s%N; }
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
for i in 1 2 3; do
  for f in $FILES; do printf '/* %s */\n' $i >> $f; done
  git -c core.hooksPath=/dev/null commit -q -am "c$i"
  s=$(now); LAYR_TRACE=1 $L git adopt 2>/tmp/ad.$i >/dev/null; echo "adopt $i: $(( ($(now)-s)/1000000 )) ms"
done
sort -k2 -n -r /tmp/ad.3 | grep -v "open project\|project " | head -25
cd ~/gitview/kernel-src; $L line delete -D $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
