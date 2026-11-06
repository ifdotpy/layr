#!/bin/bash
# ci: macos overlayfs
# Client tracing and malformed environments through the real daemon socket.
. "$(dirname "$0")/lib.sh"
set -e
env -u LAYR_TRACE "$LAYR" daemon --socket "$T/trace.sock" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill "$DPID" 2>/dev/null || true; wait "$DPID" 2>/dev/null || true; cleanup' EXIT
for i in $(seq 1 100); do
    [ -S "$T/trace.sock" ] && break
    kill -0 "$DPID" 2>/dev/null || { cat "$T/daemon.log"; exit 1; }
    sleep 0.05
done
python3 "$(dirname "$0")/service-trace.py" "$LAYR" "$T"
