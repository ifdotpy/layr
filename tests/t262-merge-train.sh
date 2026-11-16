#!/bin/bash
# ci: macos
# Merge train: a titled queue entry whose squash a train already landed on the target (title, Layr-Change and
# Layr-Queued-State trailers in the target's first-parent history) is done at its run, with no new state. A later
# version of the same change is no match and merges as usual.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q topic
cd "$P/topic" || exit 1
echo one > one.txt; l add one.txt; l save -qm one
ok "the entry is queued" l queue add topic -m "feat(topic): one file" --json
entry() { l queue list --into main --json | python3 -c "import json,sys; e=[e for e in json.load(sys.stdin) if e['status']=='$1'][-1]; print(e['$2'] or '')"; }
state=$(entry pending state)
change=$(entry pending change_id)
ok "the entry has a change" test -n "$change"

# The train: the squash of the entry on main, with the trailers that the merge job writes.
cd "$P/main" || exit 1
ok "a train squash lands on main" l merge "$state" --squash -m "feat(topic): one file

Layr-Change: $change
Layr-Queued-State: $state"
landed=$(l rev-parse head)
ok "the queue runs" l queue run --into main
eq "the landed entry is done" "done" "$(entry done status)"
eq "its run made no new state" "$landed" "$(l rev-parse head)"

# A later version of the same change: the train trailer names the old state, so it merges.
cd "$P/topic" || exit 1
echo two > two.txt; l add two.txt; l save -qm two
ok "the next version is queued" l queue add topic -m "feat(topic): one file" --json
cd "$P/main" || exit 1
ok "the queue runs again" l queue run --into main
no "the next version made a new state" test "$landed" = "$(l rev-parse head)"
eq "main has the new file" "two" "$(cat two.txt 2>/dev/null)"
finish
