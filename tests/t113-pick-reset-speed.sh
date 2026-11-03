#!/bin/bash
# `layr pick` writes only the paths of the change, and `layr reset --hard` to a state with the tree
# of the head moves only the head. Results stay exact: a pick that edits, adds, deletes and renames
# files gives the tree of the change; a pick that conflicts still conflicts; an equal-tree reset
# keeps every file time. LAYR_TRACE counts the paths that a pick visits.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf 'b\n' > $T/src/b.txt
printf 'c\n' > $T/src/c.txt
printf 'keep\n' > $T/src/keep.txt
mkdir -p $T/src/sub
printf 'x\n' > $T/src/sub/x.txt
# A tree with many more entries than the change touches.
for d in $(seq 1 30); do
    mkdir -p $T/src/big/d$d
    for f in $(seq 1 40); do echo "$d $f" > $T/src/big/d$d/f$f; done
done
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd $M/main || exit 1
BASE=$(l rev-parse head)
OLD=$(date -d '2001-01-01' +%s)
mt() { stat -c %Y "$1"; }

# The change: edit a file, add a file, delete a file, rename a file.
l line new -q work "$BASE"
cd $M/work || exit 1
echo changed > notes.txt
echo new > n.txt
mv c.txt d.txt
rm b.txt
l add n.txt d.txt >/dev/null
l save -qam change
CHANGE=$(l rev-parse head)

# Pick onto a line that has its own unrelated change.
l line new -q dst "$BASE"
cd $M/dst || exit 1
echo other > other.txt
l add other.txt && l save -qm other >/dev/null
ok "pick applies the change" l pick -q "$CHANGE"
eq "pick result is the change plus the own change" "Only in $M/dst: other.txt" "$(diff -rq -x .git $M/work $M/dst)"
no "a deleted file stays deleted" test -e b.txt
no "a renamed file leaves its old name" test -e c.txt
eq "a renamed file keeps its content" "c" "$(tr -d '\n' < d.txt)"
eq "the picked edit is in the work folder" "changed" "$(cat notes.txt)"
eq "status after pick is clean" "" "$(l status -s)"
has "the pick keeps the message of the picked state" "change" "$(l log -n1)"

# The pick writes only the changed paths: a file it does not change keeps its time.
l line new -q dst2 "$BASE"
cd $M/dst2 || exit 1
echo other > other.txt
l add other.txt && l save -qm other2 >/dev/null
touch -d '2001-01-01' big/d5/f5
LAYR_TRACE=1 l pick -q "$CHANGE" 2> $T/trace-pick.txt
eq "a file the pick does not change keeps its time" "$OLD" "$(mt big/d5/f5)"
eq "the picked edit has the picked content" "changed" "$(cat notes.txt)"
# The span exists on the APFS fast path only; other backends merge and skip this check.
if [ "$(uname -s)" = Darwin ]; then
    has "the trace counts the paths of the change" "pick visits 5 paths" "$(cat $T/trace-pick.txt)"
fi

# A path that both lines change is a conflict, as before.
l line new -q conflict "$BASE"
cd $M/conflict || exit 1
echo mine > notes.txt
l save -qam mine
no "a pick with a conflict exits with failure" l pick -q "$CHANGE"
has "the conflict names the edited file" "notes.txt" "$(l status -s)"
l reset -q --hard
cd $M/main || exit 1

# A reset to a state with the tree of the head moves only the head.
l line new -q rs "$BASE"
cd $M/rs || exit 1
l save --allow-empty -qm empty
EMPTY=$(l rev-parse head)
touch -d '2001-01-01' notes.txt keep.txt big/d1/f1
l reset -q --hard "$BASE"
eq "equal-tree reset moves the head" "$BASE" "$(l rev-parse head)"
eq "equal-tree reset keeps the time of a file" "$OLD" "$(mt notes.txt)"
eq "equal-tree reset keeps the time of another file" "$OLD" "$(mt big/d1/f1)"
eq "status after equal-tree reset is clean" "" "$(l status -s)"
l reset -q --hard "$EMPTY"
eq "equal-tree reset back moves the head" "$EMPTY" "$(l rev-parse head)"
eq "equal-tree reset back keeps the time of a file" "$OLD" "$(mt keep.txt)"

# A local edit is not an equal-tree reset: the content comes back.
echo dirty > keep.txt
l reset -q --hard "$BASE"
eq "reset --hard with a local edit restores the content" "keep" "$(cat keep.txt)"
finish
