#!/bin/bash
# ci: macos
# A queue pass pushes the line it merged into: `queue run --push <remote>`, or the remote of `queue.push` (the
# daemon's queue pass uses it too). Another machine sees the merged main after the pass. One user.
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
finish
