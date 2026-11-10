#!/bin/bash
# ci: macos overlayfs
# A replay must not make migration require a source that has no owner.
test "$(id -u)" -ne 0 || { echo 'Run this test as an ordinary user' >&2; exit 1; }
. "$(dirname "$0")/lib.sh"

q() {
    python3 -I - "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "$1" <<'PY'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    for row in db.execute(sys.argv[2]):
        print('|'.join(str(value) for value in row))
PY
}

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main"
l config undo.deleted 0 >/dev/null
l line new orphan main -q >/dev/null
cd "$P/orphan"
echo orphan > orphan.txt
l add orphan.txt
l save -m orphan -q
orphan=$(l rev-parse head)
cd "$P/main"
l line delete -D orphan >/dev/null
l gc --repair >/dev/null
eq "the state was freed" "0|1" "$(q "SELECT present,deleted FROM states WHERE display='$orphan'")"
l reindex >/dev/null
eq "replay keeps the absent record without a deletion flag" "0|0" "$(q "SELECT present,deleted FROM states WHERE display='$orphan'")"
id=$(q "SELECT id FROM states WHERE display='$orphan'")
before=$(q "SELECT rec FROM states WHERE id='$id'")
eq "the absent state has no owner" 0 "$(q "SELECT count(*) FROM owners WHERE state='$id'")"

# A stored owner must still block migration, even if replay gives no owner.
q "INSERT INTO owners(state,kind,holder) VALUES('$id','keep','migration-test')"
out=$(l migrate --dry-run 2>&1)
has "a missing source with an owner still blocks migration" "would refuse: 1 state(s)" "$out"
out=$(l migrate 2>&1); rc=$?
eq "migration refuses the missing owned source" 1 "$rc"
out=$(l migrate --leave-missing head 2>&1); rc=$?
eq "an exception for another state does not waive the missing source" 1 "$rc"
out=$(l migrate --leave-missing no-such-state 2>&1); rc=$?
no "an unknown exception is refused" test "$rc" -eq 0
out=$(l migrate --dry-run --leave-missing "$orphan" 2>&1)
has "dry run names the exact exception" "left unavailable by request: $id" "$out"
out=$(l migrate --leave-missing "$orphan" 2>&1); rc=$?
eq "an explicit exception permits the available states to migrate" 0 "$rc"
has "the result names the state left unavailable" "left unavailable by request: $id" "$out"
ok "the exception is recorded separately from verified states" python3 -I -c \
    'import json,sys; assert json.load(open(sys.argv[1])) == [sys.argv[2]]' \
    "$LAYR_ROOT/projects/p/meta/migrate/unavailable.json" "$id"
eq "the exception has no object tree" 0 "$(q "SELECT count(*) FROM states WHERE id='$id' AND json_extract(rec,'$.tree') IS NOT NULL")"
eq "the exception preserves its owner" 1 "$(q "SELECT count(*) FROM owners WHERE state='$id'")"
q "DELETE FROM owners WHERE state='$id' AND kind='keep' AND holder='migration-test'"

# Replay must also protect a keep whose owner row is missing.
main_id=$(q "SELECT id FROM lines WHERE name='main' AND deleted=0")
q "INSERT INTO keeps(line,name,state) VALUES('$main_id','migration-test','$id')"
eq "the keep has no stored owner row" 0 "$(q "SELECT count(*) FROM owners WHERE state='$id'")"
out=$(l migrate --dry-run 2>&1)
has "the replayed keep still requires its source" "would refuse: 1 state(s)" "$out"
q "DELETE FROM keeps WHERE line='$main_id' AND name='migration-test'"

out=$(l migrate --dry-run 2>&1)
has "dry run reports the absent unowned record" "1 absent source state(s) have no owner" "$out"
no "the dead source does not block migration" grep -q 'would refuse:' <<< "$out"
out=$(l migrate 2>&1); rc=$?
eq "migration succeeds after replay" 0 "$rc"
has "migration reports the unchanged absent record" "1 absent source state(s) have no owner" "$out"
eq "migration preserves the absent state record" "$before" "$(q "SELECT rec FROM states WHERE id='$id'")"
eq "migration does not mark the absent state present or deleted" "0|0" "$(q "SELECT present,deleted FROM states WHERE id='$id'")"
ok "the live head remains readable" l show head:notes.txt
finish
