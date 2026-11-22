#!/bin/bash
# Item 4: layr reset --hard in a fast line, and git checkout, on the kernel tree.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=rh$$
now() { date +%s%N; }
t() { local name=$1; shift; local s=$(now); LAYR_TRACE=1 "$@" >/dev/null 2>/tmp/rh.$name; echo "$name: $(( ($(now)-s)/1000000 )) ms rc=$?"; }
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
edit() { for f in $FILES; do printf '/* %s */\n' "$1" >> $f; done; }
edit 1; $L save -a -q -m s1; edit 2; $L save -a -q -m s2; edit 3; $L save -a -q -m s3
t reset-hard-1 $L reset -q --hard head~1
t reset-hard-2 $L reset -q --hard head~1
t reset-hard-fwd $L reset -q --hard $($L log --all -n 1 --format=%h -g 2>/dev/null | head -1)
echo "git status: $(git status --porcelain | wc -l) lines, layr status: $($L status --porcelain | wc -l)"
sort -k2 -n -r /tmp/rh.reset-hard-1 | grep -v "open project\|project \|dead temp" | head -15
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
