#!/bin/bash
# An automatic save is an operation that `undo` reverses: the newest record of the line is undone
# first. The working folder goes back to the state the save changed from; the head and the earlier
# pick stay. A second undo undoes that undo (the save comes back), as for every operation. A pick
# on a line with uncommitted work names the files it kept (20 at most, then the count of the rest).
. "$(dirname "$0")/lib.sh"

sample $T/src
l init u --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/u/lines
cd $P/main
S0=$(l rev-parse head)

# Line pk adds pick.txt in one named state; main picks it.
l line new -q pk $S0
cd $P/pk; echo pick > pick.txt; l add pick.txt; l save -a -m pk -q; PK=$(l rev-parse head)
cd $P/main
l pick -q $PK
PICK=$(l op log -n 1 | awk '{print $1}')
PH=$(l rev-parse head)
ok "the pick adds pick.txt" test -f pick.txt

# A probe edit saved as an automatic state (no message), then undo.
echo probe >> notes.txt; l save -q
out=$(l undo 2>&1)
eq "undo reverses the automatic save, not the pick" "save" "$(echo "$out" | awk '{print $3}')"
no "the probe is gone from notes.txt" grep -q probe notes.txt
eq "the head stays at the pick" "$PH" "$(l rev-parse head)"
ok "the pick stays" test -f pick.txt

# The undo of that undo: the save comes back, the head does not move.
l undo >/dev/null
has "a second undo redoes the save" "probe" "$(cat notes.txt)"
eq "the head still stays at the pick" "$PH" "$(l rev-parse head)"

# The pick is undone by its id; the later edit of notes.txt is later work and stays.
ok "undo of the pick by its id" l undo $PICK
eq "undo of the pick: head is the one before the pick" "$S0" "$(l rev-parse head)"
no "undo of the pick: pick.txt is gone" test -e pick.txt
has "undo of the pick keeps later work" "probe" "$(cat notes.txt)"

# A pick on a line with uncommitted work names the kept files.
cd $P/main
l line new -q pk2 $S0
cd $P/pk2; echo two > pick2.txt; l add pick2.txt; l save -a -m pk2 -q; PK2=$(l rev-parse head)
cd $P/main
echo wip >> notes.txt; echo new > wip.txt
out=$(l pick $PK2 2>&1)
has "the pick names the two kept files" "kept 2 uncommitted file(s): notes.txt, wip.txt" "$out"
eq "the kept file keeps its content" "new" "$(cat wip.txt)"
ok "the picked file is in the folder" test -f pick2.txt

# An automatic save after that dirty pick: its undo goes back to the picked content and keeps the
# kept file (the working state of the pick, not the head, is the state the save changed from).
echo probe2 >> pick2.txt; l save -q
l undo >/dev/null
eq "undo of a save after a dirty pick: the picked file is back" "two" "$(cat pick2.txt)"
eq "undo of that save keeps the kept file" "new" "$(cat wip.txt)"

# More than 20 kept files: the first 20 names, then the count of the rest. The last pick is quiet.
for i in $(seq 1 22); do echo x > many$i.txt; done
n=$(l status --porcelain | wc -l | tr -d ' ')
eq "24 uncommitted files before the last pick" "24" "$n"
l line new -q pk3 $S0
cd $P/pk3; echo three > pick3.txt; l add pick3.txt; l save -a -m pk3 -q; PK3=$(l rev-parse head)
cd $P/main
out=$(l pick -q $PK3 2>&1)
line=$(printf '%s\n' "$out" | grep '^kept ')
has "the quiet pick still names the kept files" "kept 24 uncommitted file(s): " "$line"
has "the names after the first 20 are a count" ", and 4 more" "$line"
names=${line#*: }; names=${names%, and 4 more}
eq "20 names are printed" "20" "$(printf '%s\n' "$names" | tr ',' '\n' | grep -c .)"

finish
