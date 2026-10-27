#!/bin/bash
# The daemon rejects incompatible clients before it opens the requested project.
. "$(dirname "$0")/lib.sh"
set -e
"$LAYR" daemon --socket "$T/version.sock" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill "$DPID" 2>/dev/null || true; cleanup' EXIT
for i in $(seq 1 100); do
    [ -S "$T/version.sock" ] && break
    kill -0 "$DPID" 2>/dev/null || { cat "$T/daemon.log"; exit 1; }
    sleep 0.05
done
python3 "$(dirname "$0")/store-version-daemon.py" "$LAYR" "$T"
