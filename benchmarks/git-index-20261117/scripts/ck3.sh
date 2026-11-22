#!/bin/bash
# The view's goto alone: a lab view (no .git link, no caches) on the object store of the project.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; S=$LAYR_ROOT/projects/gvkernel; LINES=$S/lines; n=cg$$; N=${1:-2000}
now() { date +%s%N; }
t() { local name=$1; shift; local s=$(now); "$@" >/tmp/ck.out 2>/tmp/ck.err; local rc=$?; printf '%-40s %7d ms rc=%s %s\n' "$name" $(( ($(now)-s)/1000000 )) $rc "$(grep '^#' /tmp/ck.out)"; [ $rc = 0 ] || head -2 /tmp/ck.err /tmp/ck.out; }
wait_head() { for i in $(seq 200); do [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] && return; sleep 0.05; done; echo "head did not follow" >&2; }
FILES=$(cd ~/gitview/kernel-src && git ls-files '*.c' | awk -v n=$N 'NR%int(60000/n)==1' | head -$N)
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
uu() { local d=$($L rev-parse "$1"); echo ${d:20:8}-${d:28:4}-${d:12:4}-${d:16:4}-${d:0:12}; }
base=$(uu head)
for f in $FILES; do printf '/* big */\n' >> $f; done; git commit -q -am big; wait_head
big=$(uu head); f1=$(echo $FILES | cut -d" " -f1)
V=/home/gitview-it/lab-$$; sudo mkdir -p $V/line $V/mnt
sudo $L fs view start --objects $S/meta/objects --state $base --line $V/line --mnt $V/mnt --sock $V/v.sock --allow-other --allow $(id -u) --owner $(id -u) --group $(id -g) --no-open >/dev/null 2>&1
for i in $(seq 50); do [ -S $V/v.sock ] && break; sleep 0.1; done
ls $V/mnt | wc -l | sed "s/^/lab view entries: /"
find $V/mnt -type f | wc -l | sed "s/^/files seen (walk): /"
for i in 1 2 3; do
  t "goto big ($(echo $FILES | wc -w) files differ)" sudo $L fs view ctl $V/v.sock goto $big
  echo "  file: $(tail -1 $V/mnt/$f1)"
  t "goto base" sudo $L fs view ctl $V/v.sock goto $base
  echo "  file: $(tail -1 $V/mnt/$f1)"
done
sudo $L fs view ctl $V/v.sock stop >/dev/null 2>&1; sleep 0.5; sudo umount -l $V/mnt 2>/dev/null; sudo rm -rf $V
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
