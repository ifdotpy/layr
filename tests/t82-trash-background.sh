#!/bin/bash
# The trash is emptied by a background process after a command: the command does not wait for
# the deletion, the trash still goes away, and gc empties it at once.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main || exit 1
fill() { mkdir -p "$LAYR_ROOT/trash/$1"; for d in $(seq 1 20); do mkdir -p "$LAYR_ROOT/trash/$1/d$d"; for i in $(seq 1 300); do : > "$LAYR_ROOT/trash/$1/d$d/f$i"; done; done; }
fill big
echo one > a.txt; l add a.txt
start=$(date +%s%N)
trace=$(LAYR_TRACE=1 l save -q -m one 2>&1)
ms=$(( ($(date +%s%N) - start) / 1000000 ))
no "the command does not empty the trash itself" sh -c 'printf %s "$1" | grep -q "empty trash"' _ "$trace"
for i in $(seq 1 200); do [ -z "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)" ] && break; sleep 0.1; done
eq "a background process empties the trash" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
ok "the command took well under the time to delete 6000 files in it (${ms} ms)" test "$ms" -lt 5000
fill again
l gc -q >/dev/null 2>&1
eq "gc empties the whole trash at once" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
finish
