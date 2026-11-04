#!/bin/bash
# ci: macos overlayfs
# A line new starts at the state that it read, also when the start line moves between the prepare
# step and the commit: it does not prepare again. The start is pinned, so it stays on disk. The
# test moves the start line while the new line waits after its prepare step
# (LAYR_TEST_LINE_NEW_PAUSE_MS).
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
eq "the new line starts at the head that the command read" "$old" "$(l rev-parse w)"
eq "the new line has the content of that head" one "$(cat $P/w/notes.txt)"
finish
