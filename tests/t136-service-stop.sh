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
finish
