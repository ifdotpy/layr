#!/bin/bash
# ci: macos overlayfs
# The project lock names its holder. A command that waits more than a second for it prints one
# line with the holder's command and pid; a holder file whose process is gone is not named (issue #192).
# Test hook: LAYR_TEST_LOCK_HOLD_MS keeps the lock this long after the holder is written.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init lock --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/lock
META=$P/meta
cd "$P/lines/main" || exit 1

LAYR_TEST_LOCK_HOLD_MS=4000 "$LAYR" line new -q hold-a main > "$T/holder.log" 2>&1 &
HOLDER=$!
for _ in $(seq 1 100); do [ -f "$META/lock.holder" ] && break; sleep 0.1; done
has "the holder file names the holder command" "layr line new -q hold-a main" "$(cat "$META/lock.holder" 2>/dev/null)"
l line new -q hold-b main 2> "$T/waiter.err"
wait $HOLDER
has "the waiter names the holder command and pid" "waiting for the project lock: held by 'layr line new -q hold-a main' (pid $HOLDER) for" "$(cat "$T/waiter.err")"
eq "the notice prints once" "1" "$(grep -c 'waiting for the project lock' "$T/waiter.err")"
has "the waiter ran after the holder" "hold-b" "$(l line list)"
ok "the holder file goes with the lock" test ! -e "$META/lock.holder"

# A command that does not wait prints nothing.
l line new -q quick main 2> "$T/quick.err"
eq "no notice without a wait" "" "$(cat "$T/quick.err")"

# A holder file whose process is gone is not named, and the lock holder is a process without a
# holder file here (a lock taken by another program).
sleep 0 &
DEAD=$!
wait $DEAD
python3 -c 'import fcntl,sys,time; f=open(sys.argv[1],"a"); fcntl.flock(f,fcntl.LOCK_EX); time.sleep(2.5)' "$META/lock" &
PYHOLD=$!
sleep 0.5
printf '{"pid":%s,"command":"layr save -m gone","line":"main","since_ms":1}' "$DEAD" > "$META/lock.holder"
l line new -q stale-waiter main 2> "$T/stale.err"
wait $PYHOLD
no "a dead holder is not named" grep -q "held by" "$T/stale.err"
no "the dead pid is not printed" grep -q "pid $DEAD" "$T/stale.err"
has "the waiter still prints the plain notice" "waiting for the project lock" "$(cat "$T/stale.err")"
has "the waiter ran" "stale-waiter" "$(l line list)"

# A command that runs through the service is named by its own command, not by `layr daemon`
# (layr#252). Root runs every command directly, so this part needs another user.
if [ "$(id -u)" != 0 ]; then
    LAYR_TEST_LOCK_HOLD_MS=3000 "$LAYR" daemon --tick 3600 --socket "$T/d.sock" > "$T/daemon.log" 2>&1 &
    DPID=$!
    for _ in $(seq 1 100); do [ -S "$T/d.sock" ] && break; sleep 0.1; done
    LAYR_SOCKET=$T/d.sock "$LAYR" line new -q hold-s main > "$T/svc.log" 2>&1 &
    SVC=$!
    for _ in $(seq 1 100); do [ -f "$META/lock.holder" ] && break; sleep 0.1; done
    l line new -q svc-waiter main 2> "$T/svc-waiter.err"
    wait $SVC
    kill $DPID 2>/dev/null; wait $DPID 2>/dev/null
    has "a service request is named by its command" "held by 'layr line new -q hold-s main'" "$(cat "$T/svc-waiter.err")"
    no "a service request is not named layr daemon" grep -q "held by 'layr daemon" "$T/svc-waiter.err"
fi

finish
