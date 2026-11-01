#!/bin/bash
# The first automatic save after a big new folder (an install into a line) copies the folder with
# one clone, not one clone per file, and the next save copies only what changed since. The
# state keeps every file. APFS keeps thin layers; other backends skip this test.
[ "$(uname -s)" = Darwin ] || { echo "$(basename "$0") (APFS only): 0 passed, 0 failed"; exit 0; }
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/g/lines/main || exit 1
l line new -q w >/dev/null
cd $LAYR_ROOT/projects/g/lines/w || exit 1

# A new folder with 300 files in 30 folders: not in the head.
for i in $(seq 1 30); do
    mkdir -p deps/p$i/lib
    for j in $(seq 1 10); do echo "v$i$j" > deps/p$i/lib/f$j.js; done
done
echo more >> notes.txt
out=$(LAYR_TRACE=1 "$LAYR" save 2>&1)
trace=$(printf '%s\n' "$out" | grep '^trace' | sed 's/  */ /g')
first=$(printf '%s\n' "$out" | tail -1 | awk '{print $NF}')
has "the new folder is cloned as one folder" "thin clone 1 new folders" "$trace"
eq "the layer lists the folder and its content" 1 "$(printf '%s\n' "$trace" | grep -c 'thin write 36[0-9] entries')"
echo again >> notes.txt
out=$(LAYR_TRACE=1 "$LAYR" save 2>&1)
trace=$(printf '%s\n' "$out" | grep '^trace' | sed 's/  */ /g')
has "the next save copies only the changed file" "thin write 1 entries" "$trace"
second=$(printf '%s\n' "$out" | tail -1)
eq "an unchanged folder makes no state" "$second" "$("$LAYR" save)"
eq "the state keeps the files of a cloned folder" 10 "$(l show "$first:deps/p7/lib" | grep -c '\.js$')"
finish
