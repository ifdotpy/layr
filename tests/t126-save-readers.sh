#!/bin/bash
# ci: macos overlayfs
# Readers of a named save's index (issue #194): a save moves the index into a new state (a clone
# of the staged tree), so no reader may depend on the index keeping its inode. These checks
# cover status after each save, move detection across saves (identity by creation time), and the
# status cache (keyed by the stage, the head and the fingerprints).
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init readers --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/readers/lines/main
cd "$P" || exit 1
for f in a b c; do printf 'file %s\n' $f > $f.txt; done
l add a.txt b.txt c.txt && l save -qm base

# A named save of a changed file: the state holds the change and the status is clean.
printf 'edited\n' > a.txt && l add a.txt && l save -qm edit
eq "status is clean after a named save" "" "$(l status --short)"
eq "the save records the change" "M	a.txt" "$(l show --name-status --format= head)"
eq "a second status reads the same" "" "$(l status --short)"

# A move staged before a named save: the move is recorded as a move.
l mv b.txt renamed-b.txt
eq "a staged move is seen" "R  b.txt -> renamed-b.txt" "$(l status -s | grep renamed)"
l save -qm rename
eq "the save records the move as a move" "R100	b.txt	renamed-b.txt" "$(l show --name-status --format= head)"
eq "status is clean after the move is saved" "" "$(l status --short)"

# A move made after a named save: move detection compares the new state with the working folder.
l mv c.txt moved-c.txt
eq "a move after a save is seen" "R  c.txt -> moved-c.txt" "$(l status -s | grep moved)"
l add moved-c.txt && l save -qm moved
eq "the save records the second move as a move" "R100	c.txt	moved-c.txt" "$(l show --name-status --format= head)"
eq "status is clean after the second save" "" "$(l status --short)"

# The status after saves agrees with a fresh read of the folder.
printf 'again\n' > renamed-b.txt && l add renamed-b.txt && l save -qm again
first=$(l status --short); second=$(l status --short)
eq "two statuses after a save agree" "$first" "$second"
eq "the working folder is clean after the save" "" "$first"

finish
