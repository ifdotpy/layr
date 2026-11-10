#!/bin/bash
# ci: macos
# A guarded worker's daemon must not run a configured local queue.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l line new -q topic main
cd ../topic || exit 1
echo topic > topic.txt
l add topic.txt
l save -qm topic
l queue add topic >/dev/null
cd ../main || exit 1
l config queue.run main
"$LAYR" daemon --socket "$T/manual.sock" --tick 1 --manual-queue > "$T/daemon.log" 2>&1 &
daemon=$!
sleep 4
ok "the manual daemon stays running" kill -0 "$daemon"
no "the manual daemon does not merge the queue" test -e topic.txt
kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
"$LAYR" daemon --socket "$T/normal.sock" --tick 1 > "$T/normal.log" 2>&1 &
daemon=$!
for _ in $(seq 1 20); do [ -f topic.txt ] && break; sleep 0.5; done
kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
ok "the normal daemon still runs the queue" test -f topic.txt
finish
