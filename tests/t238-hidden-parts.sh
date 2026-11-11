#!/bin/bash
# ci: macos
# The parts of a hidden glass (docs/overlays.md section 1a): each state of a line records its part on a hidden glass in
# the records of that glass (`hidden.part.<state>`), never in the public state record. A view of an older state shows
# the part that the state saved.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l overlay create secret --hidden -q
echo s1 > key.txt; l overlay move secret key.txt >/dev/null
echo a >> notes.txt
ok "a save with the hidden part" l save -qam one
h1=$(l rev-parse head)
echo s2 > key.txt; echo b >> notes.txt
ok "a second save" l save -qam two
h2=$(l rev-parse head)
eq "the public records never name the hidden glass" "" "$(sqlite3 "$P/meta/db/layr.sqlite" "select count(*) from records where raw like '%secret%'" | grep -v '^0$')"
has "the glass records the part of the first state" "hidden.part." "$(sqlite3 "$P/meta/overlays/secret/meta/db/layr.sqlite" "select raw from records" | grep -o 'hidden.part.[^"]*' | head -1)"
ok "a view of the first state" l view "$h1" --folder "$T/v1"
eq "it shows the part that the first state saved" s1 "$(cat "$T/v1/key.txt" 2>/dev/null)"
eq "the line shows the newest part" s2 "$(cat key.txt)"
finish
