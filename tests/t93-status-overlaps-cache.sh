#!/bin/bash
# `layr status` lists the overlaps with the other live lines. The compare of two saved states is
# cached in the project's meta folder: a second status makes fewer candidate compares, a saved
# state of one line changes its overlaps (new key), and a corrupt cache entry is ignored.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src/d
printf 'a\n' > $T/src/a.txt; printf 'b\n' > $T/src/b.txt; printf 'c\n' > $T/src/c.txt
printf 'e\n' > $T/src/d/e.txt
l init o --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/o/lines
CACHE=$LAYR_ROOT/projects/o/meta/overlap-compares
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true

# Six lines from main, each with a saved change. Some paths are shared: a.txt (l1, l2),
# c.txt (l4, l6), b.txt (l2, l3).
for n in 1 2 3 4 5 6; do
    cd $P/main || exit 1
    l line new -q l$n >/dev/null
    cd $P/l$n || exit 1
done
cd $P/l1 && echo one >> a.txt && l save -a -m one >/dev/null
cd $P/l2 && echo two >> a.txt && echo two >> b.txt && l save -a -m two >/dev/null
cd $P/l3 && echo three >> b.txt && l save -a -m three >/dev/null
cd $P/l4 && echo four >> c.txt && l save -a -m four >/dev/null
cd $P/l5 && echo five >> d/e.txt && l save -a -m five >/dev/null
cd $P/l6 && echo six >> c.txt && echo new > f.txt && l add f.txt >/dev/null && l save -a -m six >/dev/null

# The line that asks: unsaved changes to a.txt and c.txt.
cd $P/main || exit 1
l line new -q mine >/dev/null
cd $P/mine || exit 1
echo mine >> a.txt; echo mine >> c.txt

pairs() { # the overlaps of the status of this line, as "path line" rows
    l status --json | python3 -c 'import json,sys
a=json.load(sys.stdin)
for x in sorted(a["overlaps"], key=lambda x: (x["path"], x["line"])): print(x["path"], x["line"])'
}
EXPECT='a.txt l1
a.txt l2
c.txt l4
c.txt l6'

# The first status of the line is the cold one: no compare of two saved states is cached yet.
LAYR_TRACE=1 l status >/dev/null 2> $T/trace1
LAYR_TRACE=1 l status >/dev/null 2> $T/trace2
n1=$(grep -c candidates $T/trace1)
n2=$(grep -c candidates $T/trace2)
ok "the cold status makes candidate compares ($n1)" test "$n1" -gt 0
ok "the warm status makes fewer candidate compares ($n2 < $n1)" test "$n2" -lt "$n1"
ok "the compare cache holds entries" test -n "$(ls $CACHE 2>/dev/null)"

eq "status lists the overlaps of the six lines" "$EXPECT" "$(pairs)"
has "long status prints the overlap section" "a.txt (l1," "$(l status)"
eq "short status shows no overlaps" 0 "$(l status -s | grep -c 'Overlaps with')"

# A saved state of l3 that changes a.txt: its key is new, and l3 now overlaps a.txt.
cd $P/l3 && echo more >> a.txt && l save -a -m three-again >/dev/null
cd $P/mine || exit 1
eq "a new saved state of l3 changes its overlaps" "a.txt l1
a.txt l2
a.txt l3
c.txt l4
c.txt l6" "$(pairs)"

# Corrupt entries are ignored, the status is the same, and the entries are written again.
for f in $CACHE/*.json; do printf 'not a cache' > "$f"; done
eq "a corrupt cache does not change the overlaps" "a.txt l1
a.txt l2
a.txt l3
c.txt l4
c.txt l6" "$(pairs)"
# The compares that this status read are written again. The stale entry of the old state of l3
# stays corrupt: nothing reads it.
good=0
for f in $CACHE/*.json; do [ "$(head -c1 "$f")" = "{" ] && good=$((good + 1)); done
ok "the compares of this status are written again ($good)" test "$good" -ge 6

# An overlay that two lines hold: its changes are compared in the overlay's own project.
mkdir -p $T/osrc/private; printf 'note 1\n' > $T/osrc/private/notes.md; printf 'base\n' > $T/osrc/readme.txt
l init ov --from $T/osrc -q >/dev/null
Q=$LAYR_ROOT/projects/ov/lines
cd $Q/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l overlay create notes --mode follow -q >/dev/null
l overlay move -q notes private >/dev/null
l save -qm "private notes" >/dev/null
for n in p1 p2 pq; do
    cd $Q/main || exit 1
    l line new -q $n >/dev/null
done
cd $Q/p1 && echo p1 >> private/notes.md && l save -qam p1 >/dev/null
cd $Q/p2 && echo p2 >> private/notes.md && l save -qam p2 >/dev/null
cd $Q/pq && echo pq >> private/notes.md
eq "an overlay that two lines changed is compared" "private/notes.md p1
private/notes.md p2" "$(pairs)"
ok "the overlay compare is cached in the overlay project" test -n "$(ls $LAYR_ROOT/projects/ov/meta/overlays/notes/meta/overlap-compares 2>/dev/null)"

# The saved state of a line ignores a large folder with its own rules: its changes there are no
# overlap, and the compare skips them before any content compare.
mkdir -p $T/bsrc/target; for i in $(seq 1 300); do echo $i > $T/bsrc/target/f$i; done; echo keep > $T/bsrc/shared.txt
l init big --from $T/bsrc -q >/dev/null
B=$LAYR_ROOT/projects/big/lines
cd $B/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
for n in big1 bigq; do
    cd $B/main || exit 1
    l line new -q $n >/dev/null
done
cd $B/big1 || exit 1
printf 'target/\n' > .gitignore && l add .gitignore >/dev/null
for i in $(seq 1 300); do echo more >> target/f$i; done
echo more >> shared.txt && l save -a -m big1 >/dev/null
cd $B/bigq || exit 1
echo more >> shared.txt; echo more >> target/f1
eq "a large folder that the saved state ignores gives no overlap" "shared.txt big1" "$(pairs)"

# gc removes an unreadable entry.
cd $P/mine || exit 1
printf 'junk' > $CACHE/junk.json
l gc -q >/dev/null 2>&1
eq "gc removes an unreadable compare entry" 0 "$(test -e $CACHE/junk.json && echo 1 || echo 0)"
finish
