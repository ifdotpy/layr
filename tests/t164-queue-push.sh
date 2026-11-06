#!/bin/bash
# ci: macos
# A queue pass pushes the line it merged into: `queue run --push <remote>`, or the remote of `queue.push` (the
# daemon's queue pass uses it too, also a user daemon). Another machine sees the merged main after the pass. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
ok "push the project" l push archive -q
B() { env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" "$@"; }
mkdir -p "$T/b"
B clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/b/store/projects/p/lines/main
ok "the other machine has main" test -d "$Q"

l line new -q topic
cd "$P/topic" || exit 1
echo one > one.txt; l add one.txt; l save -qm one
l queue add topic >/dev/null
cd "$P/main" || exit 1
ok "queue run --push merges and pushes" l queue run --into main --push archive
(cd "$Q" && B pull origin -q >/dev/null 2>&1)
eq "the other machine sees the merged main" one "$(cat "$Q/one.txt" 2>/dev/null)"

no "queue.push must name a remote" l config queue.push '""'
l config queue.push archive
cd "$P/topic" || exit 1
echo two > two.txt; l add two.txt; l save -qm two
l queue add topic >/dev/null
cd "$P/main" || exit 1
ok "queue run uses queue.push" l queue run --into main
(cd "$Q" && B pull origin -q >/dev/null 2>&1)
eq "the second merge reached the other machine" two "$(cat "$Q/two.txt" 2>/dev/null)"

# The user daemon (the macOS LaunchAgent, not root) runs the queue pass of `queue.run` on its tick.
# The entry is queued before queue.run names main, so only a queue pass merges it.
cd "$P/topic" || exit 1
echo three > three.txt; l add three.txt; l save -qm three
l queue add topic >/dev/null
cd "$P/main" || exit 1
no "nothing merged it yet" test -f "$P/main/three.txt"
l config queue.run main >/dev/null
"$LAYR" daemon --socket "$T/daemon.sock" --tick 1 > "$T/daemon.log" 2>&1 &
daemon=$!
for _ in $(seq 1 60); do [ -f "$P/main/three.txt" ] && break; sleep 0.5; done
kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
eq "the user daemon merged the queued entry on its tick" three "$(cat "$P/main/three.txt" 2>/dev/null)"
(cd "$Q" && B pull origin -q >/dev/null 2>&1)
eq "and pushed it with queue.push" three "$(cat "$Q/three.txt" 2>/dev/null)"
finish
