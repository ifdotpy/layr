#!/bin/bash
# A new `layr daemon` takes the fast lines of the running one without an unmount: programs that
# work in a fast line meanwhile see no error (docs/layrfs-lab.md). Run as a non-root user with
# sudo; LAYR (built with layrfs-fuse), an isolated LAYR_ROOT, LAYR_SOCKET of its root daemon
# (running), and DAEMON_LOG for the new daemon's log.
set -u
: "${LAYR:?}" "${LAYR_ROOT:?}" "${LAYR_SOCKET:?}" "${DAEMON_LOG:=/tmp/layr-upgrade-daemon.log}"
export LAYR_AUTHOR_NAME=Up LAYR_AUTHOR_EMAIL=up@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
work=$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-upgrade.XXXXXX")
p=fastup-$(date +%s)-$$
mkdir -p $work/src/d; for i in $(seq 200); do printf 'line %s TODO\n' $i > $work/src/d/f$i; done
$L init $p --from $work/src -q >/dev/null
cd $LAYR_ROOT/projects/$p/lines/main; $L protect main --remove >/dev/null; $L migrate --delete-old >/dev/null
$L line new up --fast -q
V=$LAYR_ROOT/projects/$p/lines/up
old=$(pgrep -of "layr daemon --socket $LAYR_SOCKET")
# The work in the line: writes, reads, searches; every failure is counted.
fails=$work/fails; : > $fails
( cd $V; i=0; end=$((SECONDS + 12)); while [ $SECONDS -lt $end ]; do i=$((i+1))
    printf 'w %s\n' $i >> d/f1 || echo "write $i" >> $fails
    cat d/f2 >/dev/null || echo "read $i" >> $fails
    printf 'n %s\n' $i > "d/new$((i % 20))" || echo "create $i" >> $fails
    ls d >/dev/null || echo "list $i" >> $fails
  done; echo $i > $work/writes ) &
w1=$!
( cd $V; end=$((SECONDS + 12)); while [ $SECONDS -lt $end ]; do rg -c TODO d >/dev/null </dev/null || echo "rg" >> $fails; done ) &
w2=$!
# A descriptor open across the upgrade, and a shell whose folder is in the line.
exec 3< $V/d/f3
sleep 3
t0=$(date +%s%N)
sudo sh -c "env LAYR_ROOT=$LAYR_ROOT setsid $L daemon --socket $LAYR_SOCKET >/dev/null 2>>$DAEMON_LOG </dev/null &"
for i in $(seq 600); do new=$(pgrep -nf "layr daemon --socket $LAYR_SOCKET"); [ -S $LAYR_SOCKET ] && [ "$new" != "$old" ] && $L projects >/dev/null 2>&1 && break; sleep 0.05; done
t1=$(date +%s%N)
echo "the new service answers after $(( (t1 - t0) / 1000000 )) ms"
for i in $(seq 100); do kill -0 $old 2>/dev/null || break; sleep 0.1; done
kill -0 $old 2>/dev/null && { echo "FAIL: the old service still runs" >&2; exit 1; }
wait $w1 $w2
head -c 20 <&3 | grep -q 'line 3' || echo "fd read" >> $fails
exec 3<&-
grep -q "^w $(cat $work/writes)$" $V/d/f1 || echo "last write lost" >> $fails
grep -q " $V fuse" /proc/mounts || echo "mount gone" >> $fails
cd $V
$L status --porcelain > $work/status || echo "status" >> $fails
grep -q 'd/f1' $work/status || echo "status lost the change" >> $fails
$L save -a -m after-upgrade -q || echo "save" >> $fails
[ "$($L show head:d/f1 | tail -1)" = "w $(cat $work/writes)" ] || echo "saved content" >> $fails
cd $LAYR_ROOT/projects/$p/lines/main
$L line delete -D --discard up >/dev/null || echo "delete" >> $fails
if [ -s $fails ]; then echo "FAIL: $(sort $fails | uniq -c | head)" >&2; exit 1; fi
echo "PASS: fast line kept working through the service upgrade ($(cat $work/writes) write loops)"
