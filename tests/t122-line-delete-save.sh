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

# A delete of a line without unsaved changes still works and undoes.
l line new -q clean main
cd "$P/clean" || exit 1
printf 'clean\n' > c.txt && l add c.txt && l save -qm clean
cd "$P/main" || exit 1
ok "line delete of a clean line" l line delete -D clean
no "the clean line is deleted" test -e "$P/clean"

finish
