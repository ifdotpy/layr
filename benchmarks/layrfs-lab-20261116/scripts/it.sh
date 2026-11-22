#!/bin/bash
# Integration store for layr + LayrFS on kukuka-wsl: a root daemon on an isolated store.
# usage: it.sh up|down|parity
set -uo pipefail
IT=/home/layrfs-it
BIN=$HOME/layrfs-bench/src/lab/target/release/layr
export LAYR=$IT/layr LAYR_ROOT=$IT/store LAYR_SOCKET=$IT/run/layr.sock
case $1 in
  up)
    sudo mkdir -p $IT/run $IT/store; sudo chmod 755 $IT $IT/run
    sudo install -m 0755 $BIN $LAYR
    sudo pkill -f "layr daemon --socket $LAYR_SOCKET" 2>/dev/null; sleep 0.3
    sudo sh -c "env LAYR_ROOT=$LAYR_ROOT setsid $LAYR daemon --socket $LAYR_SOCKET >/dev/null 2>$IT/run/daemon.log </dev/null &"
    for i in $(seq 100); do [ -S $LAYR_SOCKET ] && break; sleep 0.05; done
    ls -l $LAYR_SOCKET ;;
  down)
    sudo pkill -f "fs view hub" ; sudo pkill -f "layr daemon --socket $LAYR_SOCKET"; mount | grep "$IT" | awk '{print $3}' | xargs -r -n1 sudo umount -l ;;
  layrfs)
    shift
    mkdir -p $HOME/lfx/lab/tmp
    cd /tmp && env "$@" TMPDIR=$HOME/lfx/lab/tmp PATH=$HOME/.local/bin:$PATH bash -x $HOME/layrfs-bench/src/lab/tests/views-layrfs.sh > $HOME/lfx/lab/layrfs.log 2>&1; rc=$?
    grep -v "^++" $HOME/lfx/lab/layrfs.log | tail -25; echo "rc=$rc" ;;
  parity)
    shift
    cd /tmp && env "$@" LAYR_TEST_OBJECT_STATES=1 PATH=$HOME/.local/bin:$PATH bash -x $HOME/layrfs-bench/src/lab/tests/views-parity.sh > $HOME/lfx/lab/parity.log 2>&1; rc=$?
    tail -25 $HOME/lfx/lab/parity.log | grep -v "^++" ; echo "rc=$rc" ;;
esac
