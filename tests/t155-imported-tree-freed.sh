#!/bin/bash
# An imported state loses its tree in the operation that drops its last tree owner (docs/ownership.md, "States
# imported from Git"): when a save moves main past an imported commit, the commit's state keeps only its record,
# git builds the tree again on read, and nothing is left for `gc --repair`.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; git add -A; git commit -qm c1; c1=$(git rev-parse HEAD)
echo two > a.txt; git add -A; git commit -qm c2; git push -q origin main
c2=$(git rev-parse HEAD)
l git clone $R it -q >/dev/null
cd $LAYR_ROOT/projects/it/lines/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l config undo.ops 1 >/dev/null
l config retention.auto 1 >/dev/null
has "the imported head keeps its tree" "would free 0 built Git state(s)" "$(l gc -n --repair 2>/dev/null)"
# main goes back to c1 (a complete tree: c2 is no layer's base), and the next operation moves c2 out of the undo ring.
l reset --hard "$c1" -q >/dev/null 2>&1
echo three > b.txt; l add b.txt >/dev/null; l save -qm three
# The automatic state that the reset saved is a thin layer over c2; the next saves move it out of the ring.
for i in 1 2 3; do echo $i > c$i.txt; l save -q --auto 2>/dev/null || true; l add c$i.txt >/dev/null; l save -qm c$i; done
has "c2 lost its last tree owner in that operation: nothing is left for gc --repair" "would free 0 built Git state(s)" "$(l gc -n --repair 2>/dev/null)"
l line new back "$c2" >/dev/null 2>&1
eq "git builds the tree again on read" two "$(cat $LAYR_ROOT/projects/it/lines/back/a.txt 2>&1)"
finish
