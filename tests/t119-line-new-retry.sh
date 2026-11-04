#!/bin/bash
# ci: macos overlayfs
# A line new that finds its start moved between the prepare step and the commit prepares again and
# starts from the new state. The start is a line name; the test moves that line while the new line
# waits after its prepare step (LAYR_TEST_LINE_NEW_PAUSE_MS).
. "$(dirname "$0")/lib.sh"

sample $T/src
l init r --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/r/lines
cd $P/main || exit 1
l line new -q topic >/dev/null
cd $P/topic || exit 1
echo one > notes.txt
l add notes.txt >/dev/null && l save -qm one >/dev/null
old=$(l rev-parse topic)

cd $P/main || exit 1
export LAYR_TEST_LINE_NEW_PAUSE_MS=2000
l line new -q w topic >/dev/null &
pid=$!
unset LAYR_TEST_LINE_NEW_PAUSE_MS
sleep 0.5

# The start line gets a new state while the new line waits.
cd $P/topic || exit 1
echo two >> notes.txt
l add notes.txt >/dev/null && l save -qm two >/dev/null
new=$(l rev-parse topic)
wait "$pid"
eq "the new line is made" 0 "$?"
ok "the start line moved" test "$old" != "$new"
eq "the new line starts at the moved head" "$new" "$(l rev-parse w)"
finish
