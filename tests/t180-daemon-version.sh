#!/bin/bash
# ci: macos
# `layr store-version` names the build of the daemon that answers on the socket, so a client sees a
# daemon that still runs an older binary of the same store format (an install without a restart).
# Linux: the daemon runs only as root, so this test runs on macOS (the user daemon).
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

out=$(LAYR_SOCKET="$T/none.sock" "$LAYR" store-version --json)
ok "no daemon: no daemon field" python3 -c 'import json,sys; assert "daemon" not in json.loads(sys.argv[1])' "$out"
"$LAYR" daemon --socket "$T/daemon.sock" --tick 3600 > "$T/daemon.log" 2>&1 &
daemon=$!
for _ in $(seq 100); do [ -S "$T/daemon.sock" ] && break; sleep 0.1; done
out=$(LAYR_SOCKET="$T/daemon.sock" "$LAYR" store-version --json)
ok "the daemon reports its binary" python3 -c '
import json,sys; v=json.loads(sys.argv[1]); assert v["daemon"]==v["binary"], v' "$out"
has "the text form names it" "daemon: format" "$(LAYR_SOCKET="$T/daemon.sock" "$LAYR" store-version)"
kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
finish
