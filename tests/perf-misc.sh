#!/bin/bash
# Performance fixes: a snapshot around a nested layer does not copy it; the trash is emptied
# in bounded steps (a command spends LAYR_TRASH_BUDGET_MS on it; gc empties all).
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l line new rel main >/dev/null
cd $P/rel

# 1. The snapshot of a line with a big nested cache layer clones around it.
rm -rf target; mkdir -p target/debug
for i in $(seq 1 3000); do echo $i > target/debug/f$i; done
l save --caches -q
echo more >> notes.txt
# A save with nothing staged still takes the scan copy of the line.
trace=$(LAYR_TRACE=1 l save -m two 2>&1)
has "the snapshot clones around the nested layer" "APFS clone around 1 nested layers" "$trace"
ms=$(echo "$trace" | sed -n 's/^trace *\([0-9]*\)\.[0-9] ms  APFS copy tree$/\1/p' | head -1)
[ "${ms:-9999}" -lt 100 ]; eq "the copy takes under 100 ms for 3000 cached files" 0 $?
eq "the cache stays in the line" 3000 "$(ls target/debug | wc -l | tr -d ' ')"
l save -a -qm two
eq "status is clean after the save" "" "$(l status --short)"

# 6. The trash is emptied in bounded steps.
mkdir -p "$LAYR_ROOT/trash/big/a" "$LAYR_ROOT/trash/big/b"
for i in $(seq 1 2000); do : > "$LAYR_ROOT/trash/big/a/f$i"; : > "$LAYR_ROOT/trash/big/b/f$i"; done
echo x >> notes.txt
LAYR_TRASH_BUDGET_MS=1 l save -a -q -m three
[ -n "$(ls -A "$LAYR_ROOT/trash/big/a" "$LAYR_ROOT/trash/big/b" 2>/dev/null)" ]
eq "a command with a short budget leaves part of the trash" 0 $?
for n in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    echo $n >> notes.txt
    LAYR_TRASH_BUDGET_MS=5 l save -a -q -m "more $n"
    [ -z "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)" ] && break
done
eq "later commands empty the trash" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
mkdir -p "$LAYR_ROOT/trash/again/d"
for i in $(seq 1 500); do : > "$LAYR_ROOT/trash/again/d/f$i"; done
LAYR_TRASH_BUDGET_MS=0 l gc -q >/dev/null 2>&1
eq "gc empties the whole trash" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"

finish
