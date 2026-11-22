#!/bin/bash
# Integration store for the git-view work on kukuka-wsl: a root daemon on an isolated store.
# usage: it.sh up|down|env
set -uo pipefail
IT=/home/gitview-it
BIN=${BIN:-$HOME/gitview/src/target/release/layr}
export LAYR=$IT/layr LAYR_ROOT=$IT/store LAYR_SOCKET=$IT/run/layr.sock
case $1 in
  up)
    sudo mkdir -p $IT/run $IT/store; sudo chmod 755 $IT $IT/run
    sudo pkill -f "layr daemon --socket $LAYR_SOCKET" 2>/dev/null; sleep 0.5
    sudo install -m 0755 $BIN $LAYR
    # The old socket file stays after pkill: wait for the new daemon, not for the old file.
    sudo rm -f $LAYR_SOCKET
    sudo sh -c "env LAYR_ROOT=$LAYR_ROOT setsid $LAYR daemon --socket $LAYR_SOCKET >/dev/null 2>$IT/run/daemon.log </dev/null &"
    for i in $(seq 100); do [ -S $LAYR_SOCKET ] && break; sleep 0.05; done
    ls -l $LAYR_SOCKET ;;
  down)
    sudo pkill -f "layr daemon --socket $LAYR_SOCKET"; sleep 0.3; mount | grep "$IT" | awk '{print $3}' | xargs -r -n1 sudo umount -l ;;
  env)
    echo "export LAYR=$LAYR LAYR_ROOT=$LAYR_ROOT LAYR_SOCKET=$LAYR_SOCKET PATH=$HOME/.local/bin:\$PATH" ;;
esac
