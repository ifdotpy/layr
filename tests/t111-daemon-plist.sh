#!/bin/bash
# ci: macos overlayfs
# The macOS LaunchAgent sets LAYR_ROOT, so the daemon serves the store that the user's commands use
# (packaging/launchd). `layr follow` warns when no daemon of this user serves the store: none runs,
# a daemon serves another store, or the daemon is an older build that does not report its store.
# The daemon checks run on both systems (on Linux the daemon runs as root, as in t07). The plist
# checks run on macOS only. The socket is in the test folder, never the real one.
. "$(dirname "$0")/lib.sh"

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PLIST=$ROOT/packaging/launchd/ws.layr.daemon.plist
NO_DAEMON='warning: no layr daemon runs for this store; follow passes run only on `layr follow --run`'
OLDER='warning: the layr daemon runs an older build; restart it'
# The socket path must stay under 104 bytes (AF_UNIX on macOS): a short home folder there.
if [ "$(uname -s)" = Darwin ]; then
    HOMEDIR=$(mktemp -d /tmp/layr-home.XXXXXX)
    export HOME=$HOMEDIR
    SOCK=$HOMEDIR/Library/Caches/layr/layr.sock
else
    HOMEDIR=$T/home
    mkdir -p $HOMEDIR
    export LAYR_SOCKET=$HOMEDIR/layr.sock
    SOCK=$LAYR_SOCKET
fi
hl() { env HOME=$HOMEDIR "$LAYR" "$@"; }

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

# Wait for a socket file (a daemon or the fake service below) to appear.
wait_socket() {
    for i in $(seq 1 40); do [ -S "$1" ] && return 0; sleep 0.25; done
    return 1
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

# 1. No socket: the warning is printed once, when the target is set, shown, or passed by hand.
out=$(hl follow origin/main 2>&1)
has "follow warns when no daemon listens" "$NO_DAEMON" "$out"
eq "the warning is printed once" 1 "$(printf '%s\n' "$out" | grep -c 'no layr daemon runs')"
out=$(hl follow 2>&1)
has "follow shows the warning too" "$NO_DAEMON" "$out"
out=$(hl follow --run 2>&1)
has "follow --run warns when no daemon listens" "$NO_DAEMON" "$out"

# 2. An older daemon: its hello reply has no store root, so the store is unknown.
rm -f "$SOCK"
python3 - "$SOCK" > $T/fake.log 2>&1 <<'PY' &
import json, os, socket, sys
path = sys.argv[1]
os.makedirs(os.path.dirname(path), exist_ok=True)
s = socket.socket(socket.AF_UNIX)
s.bind(path)
s.listen(8)
while True:
    c, _ = s.accept()
    b = json.dumps({"format": 1, "version": "0.0.0", "build": "older"}).encode()
    c.sendall(b"V" + len(b).to_bytes(4, "big") + b)
    c.close()
PY
fpid=$!
wait_socket "$SOCK"
out=$(hl follow 2>&1)
has "an older daemon is reported" "$OLDER" "$out"
eq "an older daemon is not reported as absent" 0 "$(printf '%s\n' "$out" | grep -c 'no layr daemon runs')"
kill -KILL $fpid 2>/dev/null; wait $fpid 2>/dev/null
rm -f "$SOCK"

# 3. A daemon on another store: its hello reply names that store, not the one of the line.
rm -f "$SOCK"
env LAYR_ROOT=$T/other-store "$LAYR" daemon --tick 60 > $T/other.log 2>&1 &
opid=$!
wait_socket "$SOCK"
has "the other daemon names its store in the log" "(store $T/other-store)" "$(head -1 $T/other.log)"
out=$(hl follow 2>&1)
has "a daemon on another store is reported" "$NO_DAEMON" "$out"
stop_daemon $opid
rm -f "$SOCK"

# 4. A daemon on the store of the line: no warning, and the passes run.
"$LAYR" daemon --tick 60 > $T/daemon.log 2>&1 &
dpid=$!
wait_socket "$SOCK"
ok "the daemon listens on the user socket" test -S "$SOCK"
has "the daemon's first log line names the store" "(store $LAYR_ROOT)" "$(head -1 $T/daemon.log)"
out=$(hl follow 2>&1)
eq "follow does not warn while the daemon serves its store" 0 "$(printf '%s\n' "$out" | grep -c 'layr daemon')"
cd $U; echo two > b.txt; git add b.txt; git commit -qm "remote two"; git push -q origin main
cd $P/main
ok "a pass with the daemon running succeeds" hl follow --run
ok "the pass fast-forwarded main" test -f b.txt
stop_daemon $dpid

# 5. The daemon was killed: its socket file stays behind and refuses connections.
out=$(hl follow 2>&1)
has "a socket file without a daemon warns" "$NO_DAEMON" "$out"

# The short home folder of the Darwin branch is not under $T: remove it here.
if [ "$(uname -s)" = Darwin ]; then rm -rf "$HOMEDIR"; fi
finish
