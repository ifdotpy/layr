#!/bin/bash
# Replication between two machines (two btrfs file systems, two stores): records, states,
# a live move of a line with its working content, divergence, signatures.
. "$(dirname "$0")/lib.sh"

new_fs $T/b 6G
BR=$T/b/store
peer() { echo env LAYR_ROOT=$BR $LAYR serve-peer x; }
sample $T/src
l init x --from $T/src -q >/dev/null
A=$LAYR_ROOT/projects/x/lines
cd $A/main
echo 1 >> notes.txt; l commit -qam a1
l sync -q -- $(peer)
B() { env LAYR_ROOT=$BR $LAYR "$@"; }
eq "peer has the history" "$(l log --format=%H)" "$(B --project x --line main log --format=%H)"
has "a second sync sends nothing" "states sent 0" "$(l sync -- $(peer))"

# A live move: automatic states go too; the peer adopts the line.
echo "not committed" > moving.txt; l save -q
l sync -q --auto -- $(peer)
B --project x adopt main >/dev/null
eq "the moved line has the uncommitted work" "not committed" "$(cat $BR/projects/x/lines/main/moving.txt)"

# Both machines continue the line: two heads.
cd $BR/projects/x/lines/main && B add moving.txt && B commit -qm "on B"
cd $A/main && echo 2 >> notes.txt && l commit -qam "on A"
l sync -q -- $(peer)
eq "both machines are known" "2" "$(l machines | wc -l)"
MB=$(l machines | grep -v '^\*' | awk '{print $1}')
eq "the local head stays local" "on A" "$(l log -1 --format=%s)"
eq "the other head is line@machine" "on B" "$(l log -1 --format=%s main@$MB)"
rm moving.txt
l merge -q main@$MB
eq "the heads are joined by a merge" "2" "$(l show -s --format=%p HEAD | wc -w)"
ok "with the work of B" test -f moving.txt
cd $BR/projects/x/lines/main
eq "B sees A's head as main@A" "on A" "$(B log -1 --format=%s main@$(l --project x machines | grep '^\*' | awk '{print $2}'))"
cd $A/main

# A changed record is refused.
f=$BR/projects/x/meta/log/$(cat $BR/machine/id).jsonl
cd $BR/projects/x/lines/main && echo t >> notes.txt && B commit -qam "later on B" && cd $A/main
sed -i '$ s/later on B/CHANGED/' $f
eq "a changed record stops the sync" "128" "$(l sync -q -- $(peer) >/dev/null 2>&1; echo $?)"
no "the changed record is not taken" sh -c "l log --all --format=%s | grep -q CHANGED"

# A peer with another project of the same name is refused.
new_fs $T/c 2G
mkdir -p $T/s2; echo other > $T/s2/f; LAYR_ROOT=$T/c/store l init x --from $T/s2 -q >/dev/null
eq "another project id is refused" "128" "$(l sync -q -- env LAYR_ROOT=$T/c/store $LAYR serve-peer x >/dev/null 2>&1; echo $?)"
finish
