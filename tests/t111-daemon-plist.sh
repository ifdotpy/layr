#!/bin/bash
# The macOS LaunchAgent sets LAYR_ROOT, so the daemon serves the store that the user's commands use
# (packaging/launchd). `layr follow` warns once when no daemon listens on the user socket. The
# daemon checks run on macOS (a user daemon); the plist and the warning run everywhere.
. "$(dirname "$0")/lib.sh"

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PLIST=$ROOT/packaging/launchd/ws.layr.daemon.plist
WARN='warning: no layr daemon runs for this store; follow passes run only on `layr follow --run`'
# The user socket is in the test HOME, so no real daemon answers.
mkdir -p $T/home
hl() { env HOME=$T/home "$LAYR" "$@"; }

# Stop a daemon and wait for the follow passes it forked (as in t105).
stop_daemon() {
    kill -STOP "$1" 2>/dev/null
    local n=0 busy=1 c
    while [ $busy -eq 1 ] && [ $n -lt 240 ]; do
        busy=0
        for c in $(pgrep -P "$1" 2>/dev/null); do
            case "$(ps -o stat= -p "$c" 2>/dev/null)" in Z*|"") ;; *) busy=1 ;; esac
        done
        [ $busy -eq 1 ] && sleep 0.5
        n=$((n + 1))
    done
    kill -KILL "$1" 2>/dev/null
    wait "$1" 2>/dev/null
}

if [ "$(uname -s)" = Darwin ]; then
    ok "the plist is valid" plutil -lint "$PLIST"
    ok "the plist has the LAYR_ROOT key" plutil -extract EnvironmentVariables.LAYR_ROOT raw -o - "$PLIST"
    # The install step of docs/follow.md, run on a copy.
    cp "$PLIST" $T/agent.plist
    ok "the install command sets LAYR_ROOT" plutil -replace EnvironmentVariables.LAYR_ROOT -string "$LAYR_ROOT" $T/agent.plist
    eq "the installed LAYR_ROOT is the store path" "$LAYR_ROOT" "$(plutil -extract EnvironmentVariables.LAYR_ROOT raw -o - $T/agent.plist)"
    ok "the installed plist is valid" plutil -lint $T/agent.plist
fi

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo base > a.txt; git add -A; git commit -qm base; git push -q origin main
l git clone $R gm -q >/dev/null
P=$LAYR_ROOT/projects/gm/lines
cd $P/main

# No daemon: the warning is printed once, when the target is set and when it is shown.
out=$(hl follow origin/main 2>&1)
has "follow warns when no daemon listens" "$WARN" "$out"
eq "the warning is printed once" 1 "$(printf '%s\n' "$out" | grep -c 'no layr daemon runs')"
out=$(hl follow 2>&1)
has "follow shows the warning too" "$WARN" "$out"
out=$(hl follow --run 2>&1)
has "follow --run warns when no daemon listens" "$WARN" "$out"

if [ "$(uname -s)" = Darwin ]; then
    sock=$T/home/Library/Caches/layr/layr.sock
    env HOME=$T/home "$LAYR" daemon --tick 60 > $T/daemon.log 2>&1 &
    dpid=$!
    for i in $(seq 1 40); do [ -S "$sock" ] && break; sleep 0.25; done
    ok "the daemon listens on the user socket" test -S "$sock"
    has "the daemon's first log line names the store" "(store $LAYR_ROOT)" "$(head -1 $T/daemon.log)"
    out=$(hl follow 2>&1)
    eq "follow does not warn while a daemon listens" 0 "$(printf '%s\n' "$out" | grep -c 'no layr daemon runs')"
    cd $U; echo two > b.txt; git add b.txt; git commit -qm "remote two"; git push -q origin main
    cd $P/main
    ok "a pass with the daemon running succeeds" hl follow --run
    ok "the pass fast-forwarded main" test -f b.txt
    stop_daemon $dpid
    # The daemon was killed, so its socket file stays behind and refuses connections.
    out=$(hl follow 2>&1)
    has "a socket file without a daemon warns" "$WARN" "$out"
fi

finish
