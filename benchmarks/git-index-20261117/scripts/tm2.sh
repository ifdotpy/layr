#!/bin/bash
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; T=$HOME/gitview/tmp
run() { export GIT_INDEX_FILE=$T/tm2.idx; rm -f $GIT_INDEX_FILE*; git $C read-tree HEAD
  /usr/bin/time -f "  $1: wall %e s, user %U s, sys %S s, minor faults %R, vol ctx %w" git $C add -A; unset GIT_INDEX_FILE; }
for v in fsm-off fsm-on; do
  n=tm2$v$$; cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1; cd $LINES/$n
  [ $v = fsm-off ] && C="-c core.fsmonitor=false" || C=""
  echo "== new line, $v"
  run "add -A 1"; run "add -A 2"
  cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
done
echo "== new line: cat all first (8 processes), then add -A"
n=tm2c$$; cd ~/gitview/kernel-src; $L line new $n --fast -q; cd $LINES/$n; C="-c core.fsmonitor=false"
/usr/bin/time -f "  cat all P8: wall %e s" sh -c "git ls-files -z | xargs -0 -P8 -n 2000 cat > /dev/null 2>&1"
run "add -A 1"
/usr/bin/time -f "  stat all: wall %e s" sh -c "git ls-files -z | xargs -0 stat -c %s > /dev/null"
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
