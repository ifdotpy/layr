#!/bin/bash
# Inside LayrFS with a split index: trace2 of git commit, FUSE requests of one commit, and fsync off.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=gd$$
now() { date +%s%N; }
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1
w=$LINES/$n; cd $w; out=$(readlink .git)
sock=$(findmnt -n -o SOURCE $w | sed 's/^layrfs-view://')
g=$w/.gs; cp -a $out $g; echo "/.gs" >> $g/info/exclude; echo "/.gs" >> $out/info/exclude
G() { git --git-dir=$g --work-tree=$w -c core.hooksPath=/dev/null "$@"; }
G config core.splitIndex true; G update-index --split-index; G status --porcelain >/dev/null; G status --porcelain >/dev/null
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
edit() { for f in $FILES; do printf '/* %s */\n' "$1" >> $f; done; }
edit a; G add -u
sudo $L fs view ctl $sock stats > /tmp/st1 2>&1
rm -f /tmp/gd2.perf; GIT_TRACE2_PERF=/tmp/gd2.perf G commit -q -m a
sudo $L fs view ctl $sock stats > /tmp/st2 2>&1
echo "=== regions of git commit (inside, split index)"
grep -E "region_leave" /tmp/gd2.perf | awk -F"|" "{print \$6, \$(NF-1), \$NF}" | grep -v "^ *0.00[0-4]" | head -30
grep -E "\| exit" /tmp/gd2.perf | awk -F"|" "{print \$6, \$NF}"
echo "=== FUSE requests of one git commit"
paste -d" " <(tr ',' '\n' < /tmp/st1) <(tr ',' '\n' < /tmp/st2) | head -40
echo "=== fsync on and off (5 commits each)"
for fs in default none; do
  t=0; for i in 1 2 3 4 5; do edit "$fs$i"; G add -u; s=$(now)
    if [ $fs = none ]; then G -c core.fsync=none commit -q -m x; else G commit -q -m x; fi
    t="$t $(( ($(now)-s)/1000000 ))"; done; echo "$fs: $t"
done
cd ~/gitview/kernel-src; $L line delete -D $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
