#!/bin/bash
# `layr status` compares our live folder once per call, not once per merge base. Each base gets
# the saved compare of the base and our head, combined with one live compare of our head. The
# overlaps equal the direct compare: the rows below are the 0.13.1 output. LAYR_OLD=<0.13.1 binary>
# also compares the full overlap JSON (owner and changed_at) with that binary.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src/d
printf 'a0\n' > $T/src/a.txt; printf 'b0\n' > $T/src/b.txt; printf 'x0\n' > $T/src/x.txt
printf 'e0\n' > $T/src/d/e.txt; printf 'f0\n' > $T/src/d/f.txt
printf 'r1\nr2\nr3\nr4\nr5\nr6\nr7\nr8\n' > $T/src/r.txt
l init ob --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/ob/lines
mk() { cd $P/main || exit 1; l line new -q $1 >/dev/null; }

cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true

# Three base states: S0 (init), S1 and S2 (main saves). Six lines, two from each base.
mk A; mk B
cd $P/A && echo bA >> b.txt && printf 'r1A\nr2\nr3\nr4\nr5\nr6\nr7\nr8\n' > r.txt && l save -a -m A >/dev/null
cd $P/B && rm d/f.txt && printf 'n\n' > n.txt && l add n.txt >/dev/null && l save -a -m B >/dev/null

cd $P/main || exit 1
printf 'x1\n' > x.txt; printf 'z\n' > z.txt; l add z.txt >/dev/null; l save -a -m S1 >/dev/null
mk C; mk D
cd $P/C && printf 'x3\n' > x.txt && l save -a -m C >/dev/null
cd $P/D && echo zd >> z.txt && l save -a -m D >/dev/null

cd $P/main || exit 1
printf 'x2\n' > x.txt; printf 'a2\n' > a.txt; l save -a -m S2 >/dev/null
mk E; mk F
cd $P/E && printf 'x4\n' > x.txt && l save -a -m E >/dev/null
cd $P/F && printf 'a5\n' > a.txt && l save -a -m F >/dev/null

# The line that asks: live edits only. x.txt is edited back to the S1 content, so it is changed
# against S2 and unchanged against S1.
mk mine
cd $P/mine || exit 1
printf 'a-mine\n' > a.txt; printf 'x1\n' > x.txt; rm b.txt; printf 'new\n' > m.txt

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
EXPECT='a.txt F
b.txt A
x.txt E'

# Cold: the saved compares are computed; one live compare of our head.
LAYR_TRACE=1 l status >/dev/null 2> $T/trace1
LAYR_TRACE=1 l status >/dev/null 2> $T/trace2
cold_live=$(grep -c "overlap live compare" $T/trace1)
warm_live=$(grep -c "overlap live compare" $T/trace2)
warm_miss=$(grep -c "overlap compare miss" $T/trace2)
ok "the cold status compares the live folder once ($cold_live)" test "$cold_live" -eq 1
ok "the warm status compares the live folder at most once ($warm_live)" test "$warm_live" -le 1
ok "the warm status computes no state compare ($warm_miss)" test "$warm_miss" -eq 0
eq "status lists the overlaps of the six bases" "$EXPECT" "$(pairs)"
eq "a file edited back to its base content is no overlap" 0 "$(pairs | grep -c '^x.txt C$')"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

# Renames: a rename in the live folder is paired by the direct compare, as before.
mk mine2
cd $P/mine2 || exit 1
mv r.txt s.txt
eq "a renamed file overlaps the line that changed the old name" "r.txt A
x.txt C" "$(pairs)"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the rename overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

finish
