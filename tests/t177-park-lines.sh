#!/bin/bash
# ci: macos overlayfs
# `layr park` keeps the changes as a line without a working folder, `parked.<line>.<n>` (docs/commands.md).
# park list lists those lines, park@{n} reads them, pop and apply merge them back, drop deletes the
# line, undo brings a dropped line back without a folder, and fsck and gc keep them.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
head=$(l rev-parse head)

echo first >> notes.txt
l park -q -m first
eq "park resets the working folder" "" "$(l status -s)"
eq "the park is a line" "parked.main.1" "$(l line list --json | python3 -c 'import json,sys; print(" ".join(sorted(x["name"] for x in json.load(sys.stdin) if x["name"].startswith("parked."))))')"
no "the parked line has no working folder" test -e $P/parked.main.1
eq "its head is a child of the line head" "$head" "$(l rev-parse 'parked.main.1~1')"
has "its head holds the change" "first" "$(l show parked.main.1:notes.txt)"

echo second > second.txt; l add second.txt
l park -q
has "a second park is a second line" "park@{0}: parked.main.2: WIP on main:" "$(l park list)"
has "park list names both, newest first" "park@{1}: parked.main.1: On main: first" "$(l park list)"
eq "park@{0} is the newest line head" "$(l rev-parse parked.main.2)" "$(l rev-parse 'park@{0}')"
eq "park@{1} is the older line head" "$(l rev-parse parked.main.1)" "$(l rev-parse 'park@{1}')"
has "park show lists the change" "second.txt" "$(l park show)"

eq "fsck finds no problem with lines without a folder" "0 problem(s)" "$(l fsck | tail -1)"
ok "save --all-lines skips the parked lines" l save -q --all-lines
ok "gc --repair runs" l gc --repair -q
has "gc keeps the parked state" "first" "$(l show 'park@{1}:notes.txt')"

# A parked line of another line does not show here.
l line new -q other
(cd $P/other && echo other > other.txt && l add other.txt && l park -q)
eq "park list shows the parks of this line only" "2" "$(l park list | wc -l | tr -d ' ')"
eq "the other line has its own" "park@{0}: parked.other.1" "$(cd $P/other && l park list | cut -d: -f1-2)"

# pop: the newest comes back and its line goes.
l park pop -q
eq "pop brings the newest back; a new file comes back untracked" "?? second.txt" "$(l status -s)"
no "pop deletes the parked line" l rev-parse parked.main.2
eq "one park is left" "1" "$(l park list | wc -l | tr -d ' ')"
l add second.txt; l save -qm second

# apply keeps the entry; drop deletes it, and undo brings it back without a folder.
l park apply -q
has "apply brings the change back" "first" "$(cat notes.txt)"
eq "apply keeps the parked line" "1" "$(l park list | wc -l | tr -d ' ')"
l reset -q --hard
has "drop names the line" "Dropped park@{0} parked.main.1" "$(l park drop)"
eq "no park is left" "" "$(l park list)"
ok "undo of the drop" l undo
eq "undo brings the parked line back" "park@{0}: parked.main.1: On main: first" "$(l park list)"
no "and gives it no folder" test -e $P/parked.main.1
eq "fsck after the undo" "0 problem(s)" "$(l fsck | tail -1)"

# park with a pop conflict keeps the entry.
echo conflict-a > c.txt; l add c.txt; l park -q
echo conflict-b > c.txt; l add c.txt; l save -qm b
no "a conflicting pop fails" l park pop -q
has "and keeps the entry" "parked.main.2" "$(l park list)"
l reset -q --hard
l park clear
eq "clear deletes every parked line of this line" "" "$(l park list)"
eq "the parked line of the other line stays" "1" "$(cd $P/other && l park list | wc -l | tr -d ' ')"

# A park can be undone: the changes come back and the parked line goes.
echo undo-me >> notes.txt
l park -q
ok "undo of a park" l undo
has "the change is back" "undo-me" "$(cat notes.txt)"
eq "and the parked line is gone" "" "$(l park list)"
eq "fsck at the end" "0 problem(s)" "$(l fsck | tail -1)"
finish
