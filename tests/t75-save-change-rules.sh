#!/bin/bash
# Plain save decides "is there a change" by what status reports (content, type, link target,
# executable bit, added and removed files) on every backend: a new time, a new mode that keeps
# the executable bit, or an empty folder make no automatic state.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/g/lines/main || exit 1
l line new -q w >/dev/null
cd $LAYR_ROOT/projects/g/lines/w || exit 1

id() { printf %s "$1" | awk '{print $NF}'; }
echo more >> notes.txt
base=$(id "$(l save)")
eq "a changed file makes a state" 1 "$(printf %s "$base" | grep -c '^[0-9a-f]\{32\}$')"
sleep 1
touch notes.txt
eq "touch makes no state" "w $base" "$(l save)"
chmod 640 notes.txt
eq "chmod 640 makes no state" "w $base" "$(l save)"
mkdir empty
eq "an empty folder makes no state" "w $base" "$(l save)"
chmod 755 empty
eq "a folder mode makes no state" "w $base" "$(l save)"
rmdir empty
eq "removing the empty folder makes no state" "w $base" "$(l save)"
chmod 755 notes.txt
new=$(id "$(l save)")
eq "the executable bit makes a state" 1 "$(printf %s "$new" | grep -c '^[0-9a-f]\{32\}$')"
[ "$new" != "$base" ] && PASS=$((PASS + 1)) || { FAIL=$((FAIL + 1)); echo "FAIL the state is new"; }
touch notes.txt
eq "touch again makes no state" "w $new" "$(l save)"
finish
