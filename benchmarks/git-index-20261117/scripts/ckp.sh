#!/bin/bash
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=ckp$$; T=$HOME/gitview/tmp
now() { date +%s%N; }; ms() { echo $(( ($(now)-$1)/1000000 )); }
wait_head() { for i in $(seq 200); do [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] && return; sleep 0.05; done; }
FILES=$(cd ~/gitview/kernel-src && git ls-files '*.c' | awk 'NR%48==1' | head -1260)
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
sock=$(findmnt -n -o SOURCE $LINES/$n | sed 's/^layrfs-view://')
stv() { sudo $L fs view ctl $sock stats | python3 -c "import json,sys; d=json.load(sys.stdin); r=d['requests']; print(' '.join(f'{k}={v}' for k,v in r.items() if k not in ('ops','write_us','read_us')), r['ops'], 'copy_ups', d['copy_ups'])"; }
git status --porcelain >/dev/null
for f in $FILES; do printf '/* big */\n' >> $f; done; git commit -q -am big; wait_head; git branch -f big; git checkout -q -b base HEAD~1; wait_head
git checkout -q big; wait_head; git checkout -q base; wait_head
echo "before: $(stv)"
s=$(now); GIT_TRACE2_PERF=$T/ckp.perf git checkout -q big; echo "checkout: $(ms $s) ms"; echo "after:  $(stv)"
grep -E "region_leave" $T/ckp.perf | awk -F"|" "{print \$6, \$(NF-1), \$NF}" | sort -rn | head -8
wait_head
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
