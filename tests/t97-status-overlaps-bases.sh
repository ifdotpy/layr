#!/bin/bash
# ci: macos overlayfs
# `layr status` compares our live folder once per call, not once per merge base, and reads each
# state tree once for all the lines. Each base gets the saved compare of the base and our head,
# combined with one live compare of our head. The overlaps equal the direct compare: the rows below
# are the 0.13.1 output. LAYR_OLD=<0.13.1 binary>
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
# Each state tree is read once for all the lines (`Repo::tree`): S0, S1, S2 and the heads of A to F,
# though the seven other lines look up 14 trees (their merge base and their state).
eq "the warm status reads each of the 9 state trees once" 9 "$(grep -c 'overlap state read$' $T/trace2)"
has "the warm status looks up 14 trees" "overlap state trees (14 calls)" "$(cat $T/trace2)"
eq "status lists the overlaps of the six bases" "$EXPECT" "$(pairs)"
eq "a file edited back to its base content is no overlap" 0 "$(pairs | grep -c '^x.txt C$')"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

# Renames combine like other changes: neither a live nor a saved rename needs the direct compare.
mk mine2
cd $P/mine2 || exit 1
mv r.txt s.txt
LAYR_TRACE=1 l status >/dev/null 2> $T/trace3
eq "a live rename needs no direct compare of a base" 0 "$(grep -c 'overlap direct base compare' $T/trace3)"
eq "a renamed file overlaps the line that changed the old name" "r.txt A
x.txt C" "$(pairs)"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the rename overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi
l add s.txt >/dev/null && l save -a -m moved >/dev/null
printf 'x1\n' > x.txt
mv s.txt t.txt
LAYR_TRACE=1 l status >/dev/null 2> $T/trace4
eq "a saved rename needs no direct compare of a base" 0 "$(grep -c 'overlap direct base compare' $T/trace4)"
eq "a rename saved and renamed again overlaps the line that changed the first name" "r.txt A
x.txt E" "$(pairs)"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the saved rename overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

# When a rule file changes between our head and the live folder, the two parts left out different
# ignored paths: they do not combine, and each path that the other line changed is compared on its
# own instead. No base falls back to the direct base compare. Each line below adds a file that our
# line renames into or out of an ignored path. `traced` keeps the trace of one status.
traced() { LAYR_TRACE=1 l status >/dev/null 2> $T/trace-ignore; }
direct() { grep -c 'overlap direct base compare' $T/trace-ignore; }
checks() { grep -c 'overlap rule change checks paths' $T/trace-ignore; }
adds() { mk $1; cd $P/$1 || exit 1; printf 'theirs\n' > $2 && l add $2 >/dev/null && l save -a -m $1 >/dev/null; }
adds adds bb.txt
adds adds2 b2.txt
adds adds3 b3
adds adds4 b4.txt

# 1. Our head saved d/e.txt -> bb.txt with bb.txt ignored (added with -f). Live, the rule is gone and
# bb.txt moves on to cc.txt: bb.txt is in neither our base nor our live folder.
mk mine3
cd $P/mine3 || exit 1
printf 'bb.txt\n' > .gitignore
mv d/e.txt bb.txt
l add .gitignore >/dev/null && l add -f bb.txt >/dev/null && l save -a -m ignored >/dev/null
rm .gitignore
mv bb.txt cc.txt
traced
ok "a changed rule file checks paths ($(checks))" test "$(checks)" -ge 1
eq "a changed rule file takes no direct base compare" 0 "$(direct)"
eq "a path ignored in our head and renamed live is no overlap" 0 "$(pairs | grep -c '^bb.txt adds$')"

# 2. Our head saved d/f.txt -> b2.txt with b2.txt ignored. Live, the rule is gone, b2.txt stays and
# another file moves: b2.txt is new against the base, so the line that adds it overlaps.
mk mine4
cd $P/mine4 || exit 1
printf 'b2.txt\n' > .gitignore
mv d/f.txt b2.txt
l add .gitignore >/dev/null && l add -f b2.txt >/dev/null && l save -a -m ignored >/dev/null
rm .gitignore
mv r.txt r2.txt
traced
ok "a removed rule checks paths" test "$(checks)" -ge 1
eq "a removed rule takes no direct base compare" 0 "$(direct)"
eq "a path ignored in our head and kept live overlaps" 1 "$(pairs | grep -c '^b2.txt adds2$')"
if [ -n "${LAYR_OLD:-}" ]; then
    eq "the ignored-path overlaps equal the old binary (full JSON)" "$(ov_json "$LAYR_OLD")" "$(ov_json "$LAYR")"
fi

# 3. Our head saved x.txt -> b3 as a file. Live, b3 is a folder and the rule b3/ ignores it. (0.50.0
# reported b3 here: its merge kept the saved file b3.)
mk mine5
cd $P/mine5 || exit 1
mv x.txt b3
l add b3 >/dev/null && l save -a -m moved >/dev/null
rm b3 && mkdir b3 && printf 'in\n' > b3/in
printf 'b3/\n' > .gitignore
traced
ok "an added rule checks paths" test "$(checks)" -ge 1
eq "an added rule takes no direct base compare" 0 "$(direct)"
eq "a file replaced by an ignored folder is no overlap" 0 "$(pairs | grep -c '^b3 adds3$')"

# Without a rule file change, the parts combine.
l add .gitignore >/dev/null && l save -a -m rule >/dev/null
printf 'more\n' > b3/more
traced
eq "an unchanged rule file checks no paths" 0 "$(checks)"
eq "an unchanged rule file takes no direct base compare" 0 "$(direct)"
eq "an ignored folder stays no overlap" 0 "$(pairs | grep -c '^b3 adds3$')"

# 4. The local exclude (.git/info/exclude, outside the tree) ignores b4.txt while our head saves
# d/e.txt -> b4.txt (added with -f), and a status saves the compares. Live, the exclusion is gone
# and b4.txt moves on to c4.txt: the saved compare made under the old exclude is not used.
mk mine6
cd $P/mine6 || exit 1
mkdir -p .git/info && printf 'b4.txt\n' > .git/info/exclude
mv d/e.txt b4.txt
l add -f b4.txt >/dev/null && l save -a -m excluded >/dev/null
l status >/dev/null
: > .git/info/exclude
mv b4.txt c4.txt
eq "a path excluded locally in our head and renamed live is no overlap" 0 "$(pairs | grep -c '^b4.txt adds4$')"

# 5. An overlay: its saved part has no rule files of the main tree and its live part (the overlay
# head against the whole folder) has them all, so it checks paths. A repeated status takes no
# direct base compare either.
mkdir -p $T/vsrc/private; printf 'note\n' > $T/vsrc/private/notes.md; printf 'build/\n' > $T/vsrc/.gitignore
l init ovr --from $T/vsrc -q >/dev/null
V=$LAYR_ROOT/projects/ovr/lines
cd $V/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l overlay create notes -q >/dev/null
l overlay move -q notes private >/dev/null
l save -qm "private notes" >/dev/null
for n in v1 vq; do cd $V/main || exit 1; l line new -q $n >/dev/null; done
cd $V/v1 && echo v1 >> private/notes.md && l save -qam v1 >/dev/null
cd $V/vq && echo vq >> private/notes.md && l save -qam vq >/dev/null && echo live >> private/notes.md
eq "an overlay that both lines changed overlaps" "private/notes.md v1" "$(pairs)"
traced
ok "the overlay checks paths ($(checks))" test "$(checks)" -ge 1
eq "a repeated overlay status takes no direct base compare" 0 "$(direct)"
eq "the overlay still overlaps" "private/notes.md v1" "$(pairs)"

finish
