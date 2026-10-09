#!/bin/bash
# The operation log, undo (and undo of undo), line delete and rename, tags, retention and the
# undo window, and fsck.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init u --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/u/lines
cd $P/main

echo a >> notes.txt; l commit -qam c1
echo b >> notes.txt; l commit -qam c2
h2=$(l rev-parse HEAD)
l reset -q --hard HEAD~2
eq "reset moved back" "initial state" "$(l log -1 --format=%s)"
l undo >/dev/null
eq "undo of reset: head" "$h2" "$(l rev-parse HEAD)"
eq "undo of reset: content" "b" "$(tail -1 notes.txt)"
l undo >/dev/null
eq "undo of the undo" "initial state" "$(l log -1 --format=%s)"
l undo >/dev/null
eq "and back again" "$h2" "$(l rev-parse HEAD)"

echo c3 >> notes.txt; l commit -qam c3
echo uncommitted > wip.txt
l undo >/dev/null
eq "undo of a commit keeps the files" "c2" "$(l log -1 --format=%s)"
eq "the undone commit's change is now uncommitted" " M notes.txt" "$(l status -s | grep notes)"
ok "untracked work survives" test -f wip.txt
l commit -qam "c3 again"
l reset -q --hard HEAD~1
R2=$(l op log -n 1 | awk '{print $1}')
echo later > later.txt
l undo >/dev/null
eq "undo of reset keeps work made after it" "later" "$(cat later.txt)"
eq "undo of reset restores the head" "c3 again" "$(l log -1 --format=%s)"
rm later.txt wip.txt

# merge then undo
l branch -q w; cd $P/w; echo w > w.txt; l add w.txt; l commit -qm w; cd $P/main
echo m > m.txt; l add m.txt; l commit -qm m
l merge -q w
ok "merged" test -f w.txt
l undo >/dev/null
no "undo of merge removes the merged file" test -e w.txt
eq "undo of merge: head" "m" "$(l log -1 --format=%s)"

# line delete and rename, tags
l branch -D w >/dev/null
no "line deleted" test -e $P/w
l undo >/dev/null
eq "undo of a line delete brings it back" "w" "$(cat $P/w/w.txt)"
l branch -m w w2
ok "renamed line folder" test -d $P/w2
l undo >/dev/null
ok "undo of a rename" test -d $P/w
l tag v1; l undo >/dev/null
eq "undo of a tag" "" "$(l tag)"

ok "op log lists operations" test "$(l op log -n 50 | grep -c ' reset')" -ge 2
has "an undone operation is marked" "(undone)" "$(l op log -n 50)"

# A push cannot be undone (the commit is on the remote).
git init -q --bare $T/r.git
l remote add origin $T/r.git; l push -q origin t1
has "undo of a push is refused" "cannot be undone" "$(l undo "$(l op log --all -n 1 | awk '{print $1}')" 2>&1)"

# Retention: keep 3 automatic states per line, then gc.
l config retention.auto 3; l config retention.hours 0
for i in 1 2 3 4 5 6; do echo $i > auto.txt; l save -q; done
n_before=$(l log --auto --format=%h | wc -l)
l config undo.days 0
l gc -q
n_after=$(l log --auto --format=%h -n 100 | while read h; do l show -s --format=%h $h >/dev/null 2>&1 && echo $h; done | wc -l)
ok "gc deletes old automatic states" test "$n_after" -lt "$n_before"
eq "line history stays" "initial state" "$(l log --format=%s | tail -1)"
ok "tagged and pushed states stay" l show -s HEAD
# The undo window is over: an operation whose states are gone cannot be undone.
has "undo after retention explains" "retention" "$(l undo --force $R2 2>&1)"
eq "fsck is clean" "0 problem(s)" "$(l fsck | tail -1)"
# A snapshot without a record (a crash between the two steps) is found and removed.
btrfs subvolume snapshot -r $P/main $LAYR_ROOT/projects/u/states/01999999-0000-7000-8000-000000000000 >/dev/null
eq "fsck finds it" "1 problem(s)" "$(l fsck | tail -1)"
l fsck --repair >/dev/null
eq "fsck repairs it" "0 problem(s)" "$(l fsck | tail -1)"
# The derived tables are rebuilt from the records.
before=$(l log --format=%H | md5sum); lines=$(l branch -v | md5sum)
l reindex >/dev/null
eq "history after a rebuild from the records" "$before" "$(l log --format=%H | md5sum)"
eq "lines after a rebuild from the records" "$lines" "$(l branch -v | md5sum)"
finish
