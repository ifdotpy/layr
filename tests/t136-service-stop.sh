#!/bin/bash
# ci: macos
# A client of the service gets an exit code and the cause, not a bare closed connection (layr#252):
# - a stopping service (SIGTERM) takes no new request and lets the running ones finish; the service
#   manager stops the rest of its process group when it exits (simulated here);
# - a request that panics returns an error and code 101.
# The daemon runs as this user (user mode), so the test needs a user other than root.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init stop --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/stop/lines/main" || exit 1
export LAYR_SOCKET=$T/d.sock
# The daemon leads its own process group, as under launchd or systemd.
start_daemon() {
    env "$@" perl -e 'setpgrp(0, 0); exec @ARGV' "$LAYR" daemon --tick 3600 --socket "$LAYR_SOCKET" >> "$T/daemon.log" 2>&1 &
    DPID=$!
    for _ in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
}

start_daemon LAYR_TEST_LOCK_HOLD_MS=3000
"$LAYR" line new -q during-stop main > "$T/req.log" 2>&1 &
REQ=$!
for _ in $(seq 1 100); do [ -f "$LAYR_ROOT/projects/stop/meta/lock.holder" ] && break; sleep 0.1; done
kill -TERM "$DPID"
for _ in $(seq 1 20); do [ -S "$LAYR_SOCKET" ] || break; sleep 0.1; done
ok "a stopping service closes its socket" test ! -S "$LAYR_SOCKET"
wait "$DPID"
# What launchd and systemd (KillMode=mixed) do when the service exits.
kill -KILL -- "-$DPID" 2>/dev/null
wait "$REQ"
rc=$?
[ $rc -eq 0 ] || cat "$T/req.log"
eq "the running request finishes" 0 "$rc"
has "its line exists" "during-stop" "$(unset LAYR_SOCKET; l line list)"
has "the service logs the stop" "stopping after 1 running requests" "$(cat "$T/daemon.log")"

start_daemon LAYR_TEST_PANIC=status
out=$("$LAYR" status 2>&1)
rc=$?
kill -TERM "$DPID"; wait "$DPID"
eq "a panic gives exit code 101" 101 "$rc"
has "the client sees the panic" "test panic in status" "$out"
has "the client sees that the command failed in the service" "stopped on an internal error (a panic)" "$out"
no "the connection is not cut" grep -q "closed the connection" <<< "$out"

# A service manager can kill the process group while a request is active. The client must
# report why it did not receive an exit code, rather than returning a bare EOF.
start_daemon LAYR_TEST_LOCK_HOLD_MS=10000
"$LAYR" line new -q interrupted main > "$T/interrupted.log" 2>&1 &
REQ=$!
for _ in $(seq 1 100); do [ -f "$LAYR_ROOT/projects/stop/meta/lock.holder" ] && break; sleep 0.1; done
if [ ! -f "$LAYR_ROOT/projects/stop/meta/lock.holder" ]; then
    kill -TERM "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null
    wait "$REQ" 2>/dev/null
    echo 'FATAL: request did not acquire the project lock'; exit 1
fi
# python, not ps: a check sandbox refuses the setuid ps.
group=$(python3 -c 'import os,sys; print(os.getpgid(int(sys.argv[1])))' "$DPID")
if [ "$group" != "$DPID" ]; then
    kill -TERM "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null
    wait "$REQ" 2>/dev/null
    echo 'FATAL: daemon does not own its process group'; exit 1
fi
kill -KILL -- "-$DPID" 2>/dev/null
wait "$DPID" 2>/dev/null
wait "$REQ"; rc=$?
no "a killed service does not report success" test "$rc" -eq 0
has "a killed service gives a useful error" "the layr service closed the connection before the command finished" "$(cat "$T/interrupted.log")"
has "the error points to the service log" "see the log of the layr service" "$(cat "$T/interrupted.log")"

# The service reaps completed request children while it continues to serve requests.
export LAYR_SOCKET=$T/reap.sock
start_daemon
for _ in 1 2 3 4 5; do
    if ! "$LAYR" status >"$T/reap.log" 2>&1; then
        cat "$T/reap.log"
        kill -TERM "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null
        echo 'FATAL: service request failed'; exit 1
    fi
done
sleep 2
# A check sandbox refuses the setuid ps; ci.yml's macos job and Linux run this part.
if ! in_check_sandbox; then
    zombies=$(ps -A -o ppid=,stat= | awk -v parent="$DPID" '$1 == parent && $2 ~ /^Z/ { n++ } END { print n+0 }')
    eq "the running service reaps completed request children" 0 "$zombies"
fi
kill -TERM "$DPID"; wait "$DPID"
finish
