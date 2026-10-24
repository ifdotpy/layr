#!/bin/bash
# Migrate old signed evidence without changes to state or record identity.
: "${OLD_LAYR:?Set OLD_LAYR to the previous Linux binary}"
. "$(dirname "$0")/lib.sh"
sample "$T/src"
"$OLD_LAYR" init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
STATE=$("$OLD_LAYR" rev-parse HEAD)
"$OLD_LAYR" approve -q HEAD
"$OLD_LAYR" try -q HEAD -- true
"$OLD_LAYR" approve -q --withdraw HEAD
DB=$LAYR_ROOT/projects/p/meta/db/layr.sqlite
OLD_SIG=$(python3 - "$DB" <<'PY'
import hashlib,sqlite3,sys
c=sqlite3.connect(sys.argv[1])
print(hashlib.sha256(''.join(x[0] for x in c.execute("select raw from records order by machine,seq")).encode()).hexdigest())
PY
)
eq 'old states resolve without ChangeId' "$STATE" "$(l rev-parse head)"
eq 'old approvals migrate' 1 "$(l attestations head --kind approval --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
eq 'old checks migrate' 1 "$(l attestations head --kind check --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
eq 'old withdrawals migrate' 1 "$(l attestations head --kind revoke --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
NEW_SIG=$(python3 - "$DB" <<'PY'
import hashlib,sqlite3,sys
c=sqlite3.connect(sys.argv[1])
print(hashlib.sha256(''.join(x[0] for x in c.execute("select raw from records order by machine,seq")).encode()).hexdigest())
PY
)
eq 'migration preserves every signed byte' "$OLD_SIG" "$NEW_SIG"
"$OLD_LAYR" approve -q HEAD
eq 'later legacy records also migrate' 2 "$(l attestations head --kind approval --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
l approve -q --withdraw head
no 'the old binary sees new withdrawals' "$OLD_LAYR" approvals HEAD
l approve -q head
ok 'the old binary reads new approvals' "$OLD_LAYR" approvals HEAD
ok 'the old binary verifies new records' "$OLD_LAYR" fsck
l save --allow-empty -qm new
NEW=$(l rev-parse head)
eq 'the old binary reads a new ordinary state' "$NEW" "$("$OLD_LAYR" rev-parse HEAD)"
"$OLD_LAYR" commit --allow-empty -qm old
ok 'the new binary accepts a later legacy state' l fsck
ok 'reindex accepts old and new records' l reindex -q
finish
