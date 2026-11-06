#!/bin/bash
# Park entries of a previous binary (states of kind stash) become parked lines on the first park
# command of the candidate binary, with their content and their order. Run by hand:
# OLD_LAYR=<previous binary> LAYR=<candidate> bash tests/compat-park.sh
: "${OLD_LAYR:?Set OLD_LAYR to the previous binary}"
. "$(dirname "$0")/lib.sh"
sample "$T/src"
"$OLD_LAYR" init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
"$OLD_LAYR" protect main --remove
echo old-first >> notes.txt; "$OLD_LAYR" park -q -m first
echo old-second > second.txt; "$OLD_LAYR" add second.txt; "$OLD_LAYR" park -q -m second
"$OLD_LAYR" line new -q w
(cd "$P/w" && echo old-w >> notes.txt && "$OLD_LAYR" park -q -m on-w)
first=$("$OLD_LAYR" rev-parse 'park@{1}')

# The candidate keeps the old entries before any park command: gc does not free them.
ok "gc --repair runs before the move" l gc --repair -q
has "the old entry keeps its tree" "old-first" "$(l show "$first:notes.txt")"

out=$(l park list 2>&1)
has "the first park command names the moved entries" "is now the line parked.main.1" "$out"
has "park list shows the newest old entry first" "park@{0}: parked.main.2: On main: second" "$out"
has "and the older one second" "park@{1}: parked.main.1: On main: first" "$out"
eq "the moved line has the old state as its head" "$first" "$(l rev-parse parked.main.1)"
eq "the entry of the other line moved too" "park@{0}: parked.w.1: On w: on-w" "$(cd "$P/w" && l park list)"
eq "a second park command moves nothing" "" "$(l park list 2>&1 >/dev/null)"
eq "fsck finds no problem" "0 problem(s)" "$(l fsck | tail -1)"
ok "gc --repair runs after the move" l gc --repair -q
l park pop -q
eq "pop of a moved entry brings the change back" "?? second.txt" "$(l status -s)"
l add second.txt; l save -qm second
l park pop -q
has "and the older one" "old-first" "$(cat notes.txt)"
eq "no entry is left on main" "" "$(l park list)"
finish
