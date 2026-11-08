#!/bin/bash
# ci: macos overlayfs
# Checkpoints of an agent tool (docs/commands.md, "Checkpoints"): `save --keep` holds an automatic
# state past the retention ring and gc --repair until `keep drop` or the line's delete;
# `diff --exclude-standard` leaves ignored build output out of a diff of two states;
# `restore --clean` puts the folder back to a state in one operation.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src/fixtures $T/src/keepdir && cd $T/src
printf 'dist/\n*.log\n' > .gitignore; echo a > a.txt; echo sample > fixtures/sample.log; echo k > keepdir/k.txt
git init -q && git add -A && git add -f fixtures/sample.log && git -c user.name=t -c user.email=t@t commit -qm base
l init k --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/k
DB=$P/meta/db/layr.sqlite
[ -f "$DB" ] || DB=$P/meta/layr.sqlite
cd $P/lines/main && l line new -q agent >/dev/null && cd $P/lines/agent || exit 1
l config retention.auto 2
kept() { l keep list --json | python3 -c 'import sys,json; print({k["name"]: k["state"] for k in json.load(sys.stdin)}.get(sys.argv[1], ""))' "$1"; }

mkdir newdir && echo a > newdir/a
l save -q --note turn0 --keep t3/turn+/0
echo b >> a.txt; echo new > new.txt; mkdir dist; echo built > dist/out.js; echo edited >> fixtures/sample.log
echo new > keepdir/new.txt; echo x > other.log; echo b > newdir/b
l save -q --note turn1 --keep t3/turn+/1
for i in 1 2 3 4; do echo $i > churn.txt; l save -q; done
rm churn.txt
before=$(kept t3/turn+/0); after=$(kept t3/turn+/1)
eq "a keep outlives the retention ring" "a" "$(l show $before:a.txt)"
has "a plain diff of two states shows ignored files" "dist/out.js" "$(l diff --numstat $before $after)"
eq "diff --exclude-standard leaves out ignored new files, not tracked ones" \
    "$(printf '1\t0\ta.txt\n1\t0\tfixtures/sample.log\n1\t0\tkeepdir/new.txt\n1\t0\tnew.txt\n1\t0\tnewdir/b')" \
    "$(l diff --numstat --exclude-standard $before $after)"
l save -q --keep same/1; l save -q --keep same/2
eq "an unchanged folder keeps the same state under both names" "$(kept same/1)" "$(kept same/2)"
# A named state that only a kept state holds (as its parent) once a reset left it and the rings moved on.
l config undo.ops 1
echo z > z.txt; l add z.txt; l save -qm named
echo zz > z.txt; l save -q --keep on-named
l reset -q --hard head~1
for i in 5 6 7; do echo $i > churn.txt; l save -q; done
rm -f churn.txt
eq "the kept state still has its parent" "z" "$(l show $(kept on-named)~1:z.txt)"
ok "the owners check finds no difference with keeps past the ring" l gc
l gc --repair >/dev/null 2>&1
eq "gc --repair keeps the kept states" "a" "$(l show $before:a.txt)"
# A replay frees nothing (docs/ownership.md): the repair after it frees what the rings dropped, never a keep.
l reindex >/dev/null 2>&1
l gc --repair >/dev/null 2>&1
eq "a reindex and a repair keep them" "a" "$(l show $(kept t3/turn+/0):a.txt)"
ok "and the owners check stays clean" l gc

ok "restore --clean puts the folder back" l restore --source $before --clean -- .
eq "the tracked file is back" "a" "$(cat a.txt)"
no "a new file is removed" test -e new.txt
no "a new file in a folder of the source is removed" test -e keepdir/new.txt
ok "the folder of the source stays" test -f keepdir/k.txt
no "a new file in a new folder that the source has is removed" test -e newdir/b
ok "the source's file in that folder stays" test -f newdir/a
ok "an ignored file stays" test -f dist/out.js
eq "it is one restore operation, with no clean" "1 0" "$(l op log -n 2 | grep -c ' restore ') $(l op log -n 2 | grep -c ' clean ')"
no "--clean needs the working folder" l restore --staged --clean --source $before -- .

ok "keep drop releases a keep" l keep drop t3/turn+/0
eq "it is gone from the list" "" "$(kept t3/turn+/0)"
no "a missing keep is an error" l keep drop t3/turn+/0
no "--keep refuses a named save" l save -m named --keep x
cd $P/lines/main
l line delete -D agent >/dev/null
eq "the line delete releases its keeps" 0 "$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("select count(*) from owners where kind=\"keep\"").fetchone()[0])' "$DB")"
finish
