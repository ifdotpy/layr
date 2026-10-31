#!/bin/bash
# Files that a command writes into the working folder get the current time, as `git checkout`
# gives them: build tools compare times, and a file with the old time of its state looks older
# than build output of other content (a false test pass). Files that stay keep their time.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd $M/main || exit 1
mt() { stat -c %Y "$1"; }
OLD=$(date -d '2001-01-01' +%s)
NEW=$(date -d '2020-01-01' +%s)
echo keep > k.txt; touch -d '2001-01-01' k.txt
echo one > a.txt; touch -d '2001-01-01' a.txt; l add a.txt k.txt; l save -qm one; l tag one
echo two > a.txt; touch -d '2001-01-01' a.txt; l save -qam two; l tag two

l restore --source one a.txt
eq "restore writes the content" "one" "$(cat a.txt)"
ok "restore gives the written file the current time" test "$(mt a.txt)" -gt "$NEW"
eq "restore leaves other files alone" "$OLD" "$(mt k.txt)"
l restore a.txt
eq "status after restore is clean" "" "$(l status -s)"

l reset -q --hard one
eq "reset --hard writes the content" "one" "$(cat a.txt)"
ok "reset --hard gives the current time" test "$(mt a.txt)" -gt "$NEW"
eq "reset --hard leaves unchanged files alone" "$OLD" "$(mt k.txt)"
eq "status after reset --hard is clean" "" "$(l status -s)"

l line new -q other two >/dev/null
cd $M/other || exit 1
echo three > a.txt; touch -d '2001-01-01' a.txt; l save -qam three
cd $M/main || exit 1
l reset -q --hard two
touch -d '2001-01-01' a.txt
l merge -q other
eq "merge writes the content" "three" "$(cat a.txt)"
ok "merge gives the current time" test "$(mt a.txt)" -gt "$NEW"
eq "status after merge is clean" "" "$(l status -s)"

echo four > a.txt; l park -q
touch -d '2001-01-01' a.txt
l park pop -q
eq "park pop writes the content" "four" "$(cat a.txt)"
ok "park pop gives the current time" test "$(mt a.txt)" -gt "$NEW"
finish
