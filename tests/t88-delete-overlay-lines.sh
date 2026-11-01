#!/bin/bash
# A deleted line takes its cache overlay lines with it: gc and fsck then find no leftover head.
# A save that cannot write its snapshot leaves no record and no snapshot.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd $P/main || exit 1
l config regenerable '["target"]'
l line new -q w >/dev/null
cd $P/w || exit 1
rm -rf target; mkdir target; echo cache > target/x
l save --caches --cache target -q >/dev/null 2>&1
echo changed >> notes.txt
l save -a -m one -q >/dev/null 2>&1
cd $P/main || exit 1
ok "the line with a cache overlay line is deleted" l line delete -D w
l gc -q >/dev/null 2>&1
eq "fsck after delete and gc finds no problem" "0 problem(s)" "$(l fsck 2>&1 | tail -1)"
eq "fsck names no overlay head of the deleted line" "" "$(l fsck 2>&1 | grep 'head of line')"

# A save whose snapshot folder is locked cannot write its snapshot: it fails, and no record names a
# state that is not there (the old code marked such a state present).
l line new -q v >/dev/null
cd $P/v || exit 1
echo more >> notes.txt
states=$LAYR_ROOT/projects/g/states
deltas=$LAYR_ROOT/projects/g/deltas
lock() { # lock <folder>...: no entry can be added (immutable)
    for d in "$@"; do [ -d "$d" ] || continue; if [ "$(uname -s)" = Darwin ]; then chflags uchg "$d"; else chattr +i "$d"; fi; done
}
unlock() {
    for d in "$@"; do [ -d "$d" ] || continue; if [ "$(uname -s)" = Darwin ]; then chflags nouchg "$d"; else chattr -i "$d"; fi; done
}
lock $states $deltas
no "a save whose snapshot folder is locked fails" l save -a -m locked -q
unlock $states $deltas
cd $P/main || exit 1
eq "fsck after the failed save finds no problem" "0 problem(s)" "$(l fsck 2>&1 | tail -1)"
finish
