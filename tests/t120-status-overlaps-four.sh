#!/bin/bash
# ci: macos overlayfs
# `layr status` lists the overlaps of four lines, two of which edit the same file (a.txt). The rows
# are the rows of the 0.21.0 binary. LAYR_OLD=<0.21.0 binary> also compares the full JSON (owner and
# changed_at). A live edit or a save shows in the next status, also after a status saved a compare.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src/d
printf 'a\n' > $T/src/a.txt; printf 'b\n' > $T/src/b.txt; printf 'c\n' > $T/src/c.txt
printf 'e\n' > $T/src/d/e.txt
l init q --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/q/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true

# Four lines from main. k1 and k2 both change a.txt.
for n in 1 2 3 4; do
    cd $P/main || exit 1
    l line new -q k$n >/dev/null
done
cd $P/k1 && echo one >> a.txt && l save -a -m one >/dev/null
cd $P/k2 && echo two >> a.txt && l save -a -m two >/dev/null
cd $P/k3 && echo three >> b.txt && l save -a -m three >/dev/null
cd $P/k4 && echo four >> d/e.txt && l save -a -m four >/dev/null

# The line that asks: unsaved changes to a.txt and b.txt.
cd $P/main || exit 1
l line new -q mine >/dev/null
cd $P/mine || exit 1
echo mine >> a.txt; echo mine >> b.txt

pairs() { # the overlaps of the status of this line, as "path line" rows
    l status --json | python3 -c 'import json,sys
a=json.load(sys.stdin)
for x in sorted(a["overlaps"], key=lambda x: (x["path"], x["line"])): print(x["path"], x["line"])'
}
ov_json() { # the full overlap objects of the status of this line, with the binary given
    "$1" status --json | python3 -c 'import json,sys
a=json.load(sys.stdin)
print(json.dumps(sorted(a["overlaps"], key=lambda x: (x["path"], x["line"])), sort_keys=True))'
}
EXPECT='a.txt k1
a.txt k2
b.txt k3'

eq "status lists the overlaps of the four lines" "$EXPECT" "$(pairs)"
eq "a second status lists the same overlaps" "$EXPECT" "$(pairs)"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

# A live edit after a status that saved the compare: the next status shows it.
cd $P/mine || exit 1
echo more >> d/e.txt
eq "a live edit to d/e.txt overlaps k4" "a.txt k1
a.txt k2
b.txt k3
d/e.txt k4" "$(pairs)"

# Undo the live edit: the row goes away again.
printf 'e\n' > d/e.txt
eq "a live edit that is undone drops its overlap" "$EXPECT" "$(pairs)"

# A save of mine moves its head: the overlaps follow the new head.
cd $P/mine || exit 1
l save -a -m mine >/dev/null
eq "after a save the overlaps of the saved changes stay" "$EXPECT" "$(pairs)"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the overlaps after a save equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

finish
