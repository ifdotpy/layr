#!/bin/bash
# ci: macos overlayfs
# `layr status` reads the overlays of the project once for all the other lines that it compares
# with, not once per line: the overlay settings and the hidden overlay projects are per command.
# With 4 or 16 other lines the status opens the project of the hidden overlay equally often.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src
printf 'a\n' > $T/src/a.txt; printf 'b\n' > $T/src/b.txt
l init q --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/q/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l overlay create secret --hidden -q
for n in 1 2 3; do l overlay create pub$n -q; done
l overlay use main -q

l line new -q k1 >/dev/null
cd $P/k1 && echo one >> a.txt && l save -qam one >/dev/null
cd $P/main && l line new -q k --count 3 >/dev/null
cd $P/main && l line new -q mine >/dev/null
cd $P/mine || exit 1
echo mine >> a.txt

opens() { # the opens of the hidden overlay's project in one status of this line
    LAYR_TRACE=1 l status 2>&1 >/dev/null | grep -c 'open project q%secret$'
}
# The first status after new lines does one-time work there: count the next one.
l status >/dev/null
few=$(opens)
ok "status opens the hidden overlay project at least once" test "$few" -gt 0
eq "status lists the overlap with k1" "a.txt k1" "$(l status --json | python3 -c 'import json,sys
for x in json.load(sys.stdin)["overlaps"]: print(x["path"], x["line"])')"
cd $P/main && l line new -q j --count 12 >/dev/null
cd $P/mine || exit 1
l status >/dev/null
eq "status with 16 other lines opens the hidden overlay as often as with 4 ($few)" "$few" "$(opens)"

finish
