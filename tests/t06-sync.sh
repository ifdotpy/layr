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
peer_exchange sync x -q -- $(peer)
B() { env LAYR_ROOT=$BR $LAYR "$@"; }
eq "peer has the history" "$(l log --format=%H)" "$(B --project x --line main log --format=%H)"
has "a second sync sends nothing" "states sent 0" "$(peer_exchange sync x -- $(peer))"

# A live move: automatic states go too; the peer adopts the line.
echo "not committed" > moving.txt; l save -q
peer_exchange sync x -q --auto -- $(peer)
B --project x adopt main >/dev/null
eq "the moved line has the uncommitted work" "not committed" "$(cat $BR/projects/x/lines/main/moving.txt)"

# Both machines continue the line: two heads.
cd $BR/projects/x/lines/main && B add moving.txt && B commit -qm "on B"
cd $A/main && echo 2 >> notes.txt && l commit -qam "on A"
peer_exchange sync x -q -- $(peer)
eq "both machines are known" "2" "$(l remote list | grep -v '^remote' | wc -l)"
MB=$(l remote list | grep -v -e '^\*' -e '^remote' | awk '{print $1}')
eq "the local head stays local" "on A" "$(l log -1 --format=%s)"
eq "the other head is line@machine" "on B" "$(l log -1 --format=%s main@$MB)"
rm moving.txt
l merge -q main@$MB
eq "the heads are joined by a merge" "2" "$(l show -s --format=%p HEAD | wc -w)"
ok "with the work of B" test -f moving.txt
cd $BR/projects/x/lines/main
eq "B sees A's head as main@A" "on A" "$(B log -1 --format=%s main@$(l --project x remote list | grep '^\*' | awk '{print $2}'))"
cd $A/main

# A changed record is refused.
cd $BR/projects/x/lines/main && echo t >> notes.txt && B commit -qam "later on B" && cd $A/main
python3 - $BR/projects/x/meta/db/layr.sqlite <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("UPDATE records SET raw = replace(raw, 'later on B', 'CHANGED') WHERE raw LIKE '%later on B%'")
c.commit()
PY
eq "a changed record stops the sync" "128" "$(peer_exchange sync x -q -- $(peer) >/dev/null 2>&1; echo $?)"
no "the changed record is not taken" sh -c "l log --all --format=%s | grep -q CHANGED"

# A peer with another project of the same name is refused.
new_fs $T/c 2G
mkdir -p $T/s2; echo other > $T/s2/f; LAYR_ROOT=$T/c/store l init x --from $T/s2 -q >/dev/null
eq "another project id is refused" "128" "$(peer_exchange sync x -q -- env LAYR_ROOT=$T/c/store $LAYR serve-peer x >/dev/null 2>&1; echo $?)"
# Records also travel as JSONL (export and import), checked like a sync.
new_fs $T/d 2G
l op export > $T/records.jsonl
eq "export holds every record" "$(l op log --all -n 100000 | wc -l)" "$(wc -l < $T/records.jsonl)"
cd $A/main
python3 - $T/records.jsonl <<'PY'
import sys
lines = open(sys.argv[1]).read().splitlines()
lines[3] = lines[3].replace('"seq":4', '"seq":4 ')
open(sys.argv[1] + ".bad", "w").write("\n".join(lines) + "\n")
PY
no "import refuses a changed record" l op import $T/records.jsonl.bad
eq "a valid import adds nothing new here" "imported 0 new record(s) of $(wc -l < $T/records.jsonl)" "$(l op import $T/records.jsonl)"
finish
