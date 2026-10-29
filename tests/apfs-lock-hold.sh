#!/bin/bash
# Short store lock holds on APFS: a command saves the working folder before the lock (the
# lock only takes that layer), and the state after the command copies only what it changed.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init hold --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/hold/lines
cd "$P/main" || exit 1
l line new -q topic
cd "$P/topic" || exit 1
printf 'tracked\n' > tracked.txt
l add tracked.txt && l save -qm tracked
mkdir -p bulk
for i in $(seq 1 300); do printf '%s\n' "$i" > "bulk/f$i"; done
printf 'edited\n' > tracked.txt

trace=$(LAYR_TRACE=1 l restore tracked.txt 2>&1)
eq "restore discards the edit" "tracked" "$(cat tracked.txt)"
has "the save before the command was written before the lock" "take prepared save" "$trace"
has "the state after the command copies only the restored file" "thin write 1 " "$(printf '%s\n' "$trace" | sed 's/  */ /g')"
ok "undo of the restore" l undo
eq "undo brings the edit back" "edited" "$(cat tracked.txt)"
eq "the untracked files stay" "300" "$(ls bulk | wc -l | tr -d ' ')"
eq "no prepared copies stay" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"

# add reads the working folder before the lock and does not read it again under the lock.
printf 'new\n' > new.txt
eq "add reads the folder once" "1" "$(LAYR_TRACE=1 l add new.txt 2>&1 | grep -c 'validated read')"
has "add stages the file" "A  new.txt" "$(l status --short)"
ok "states remain valid" l fsck
finish
