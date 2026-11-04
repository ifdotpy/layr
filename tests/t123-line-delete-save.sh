#!/bin/bash
# ci: macos overlayfs
# `line delete` saves the working state of the line before it takes the project lock (issue #194):
# unsaved changes and new files go into the automatic state, and `undo` brings them back.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init del --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/del/lines
cd "$P/main" || exit 1
l line new -q topic
cd "$P/topic" || exit 1
printf 'tracked\n' > tracked.txt
l add tracked.txt && l save -qm tracked
printf 'unsaved\n' > tracked.txt
mkdir -p bulk && for i in $(seq 1 300); do printf '%s\n' "$i" > "bulk/f$i"; done
cd "$P/main" || exit 1

trace=$(LAYR_TRACE=1 l line delete -D topic 2>&1)
no "the line is deleted" test -e "$P/topic"
has "the lock is taken after the save" "store lock taken" "$trace"
# The layer written before the lock is an APFS feature (Linux saves a snapshot instead).
[ "$(uname -s)" = Darwin ] && has "the save is written before the lock" "prepare save" "$trace"
eq "no prepared layer stays" "" "$(ls -A "$LAYR_ROOT/projects/del/meta/work" 2>/dev/null)"

ok "undo of the delete" l undo
eq "undo brings the unsaved change back" "unsaved" "$(cat "$P/topic/tracked.txt")"
eq "undo brings the new files back" "300" "$(ls "$P/topic/bulk" | wc -l | tr -d ' ')"

# A folder that changes after the prepare step (a build writes into it) is not prepared again: the
# prepared state stays as an automatic state, and the state under the lock adds only the change.
cd "$P/main" || exit 1
l line new -q busy
cd "$P/busy" || exit 1
mkdir -p bulk && for i in $(seq 1 300); do printf '%s\n' "$i" > "bulk/f$i"; done
cd "$P/main" || exit 1
LAYR_TEST_LINE_DELETE_PAUSE_MS=3000 LAYR_TRACE=1 l line delete -D busy 2>"$T/busy.trace" &
pid=$!
sleep 1
printf 'late\n' > "$P/busy/late.txt"
printf 'changed\n' > "$P/busy/bulk/f1"
wait $pid
eq "the busy line is deleted" 0 "$?"
[ "$(uname -s)" = Darwin ] && eq "the save is prepared once" 1 "$(grep -c "prepare save" "$T/busy.trace")"
ok "undo of the busy delete" l undo
eq "undo brings the file written after the prepare back" late "$(cat "$P/busy/late.txt")"
eq "undo brings the change made after the prepare back" changed "$(cat "$P/busy/bulk/f1")"
eq "undo brings the prepared files back" 300 "$(ls "$P/busy/bulk" | wc -l | tr -d ' ')"

# A delete of a line without unsaved changes still works and undoes.
l line new -q clean main
cd "$P/clean" || exit 1
printf 'clean\n' > c.txt && l add c.txt && l save -qm clean
cd "$P/main" || exit 1
ok "line delete of a clean line" l line delete -D clean
no "the clean line is deleted" test -e "$P/clean"

finish
