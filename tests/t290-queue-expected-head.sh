#!/bin/bash
# ci: macos overlayfs
# A signed target expectation survives queueing, head movement, and index rebuilds.
. "$(dirname "$0")/lib.sh"
set -e
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
SRC=$(cd "$(dirname "$0")/.." && pwd)
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
base=$(l rev-parse head)
base_uuid=$(l line list --json | python3 -c 'import json,sys; print(next(e["head"] for e in json.load(sys.stdin) if e["name"]=="main"))')
l line new -q first
cd ../first
echo first > first.txt
l add first.txt; l save -qm 'feat: first'
first=$(l rev-parse head)
first_uuid=$(l line list --json | python3 -c 'import json,sys; print(next(e["head"] for e in json.load(sys.stdin) if e["name"]=="first"))')
l line new -q second
cd ../second
echo second > second.txt
l add second.txt; l save -qm 'feat: second'
second=$(l rev-parse head)
second_uuid=$(l line list --json | python3 -c 'import json,sys; print(next(e["head"] for e in json.load(sys.stdin) if e["name"]=="second"))')
cd ../main
l queue add first --into main --expect-head "$base" --no-run-queue --json > "$T/first.json"
l queue add second --into main --expect-head "$base" --no-run-queue --json > "$T/second.json"
ok 'queue add records the exact expected target' python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["expected_head"] == sys.argv[2]' "$T/second.json" "$base_uuid"
ok 'the pending expectations survive reindex' l reindex -q
rc=0
l queue run --into main --json > "$T/result.json" || rc=$?
eq 'the second entry fails after the first moves main' 1 "$rc"
eq 'main contains only the first entry' "$first" "$(l rev-parse head)"
no 'the rejected entry installs no files' test -e second.txt
ok 'the queue records a typed failure with both heads' python3 -c 'import json,sys; a,b=json.load(open(sys.argv[1])); assert a["status"] == "done"; assert b["status"] == "failed"; assert b["attempts"] == 1; assert b["failure"] == {"expected":sys.argv[2],"found":sys.argv[3]}; assert b["reason"] == "target head mismatch: expected " + sys.argv[2] + ", found " + sys.argv[3]; assert "result" not in b' "$T/result.json" "$base_uuid" "$first_uuid"
ok 'the failure matches the JSON schema' python3 "$SRC/tests/json-schema.py" check "$SRC/schemas/queue-list.json" "$T/result.json" --strict
ok 'the typed failure survives reindex' l reindex -q
l queue list --json > "$T/rebuilt.json"
eq 'reindex keeps the queue entries' "$(cat "$T/result.json")" "$(cat "$T/rebuilt.json")"
# A stale checked-run guard must still fail a pinned entry under the target lock.
l line new -q guarded "$first"
entry=$(l queue add second --into guarded --expect-head "$first" --no-run-queue --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
l --line guarded save --allow-empty -qm concurrent
found=$(l line list --json | python3 -c 'import json,sys; print(next(e["head"] for e in json.load(sys.stdin) if e["name"]=="guarded"))')
rc=0
l queue run --into guarded --checked-entry "$entry" --checked-head "$first" --checked-state "$second" --json > "$T/guarded.json" || rc=$?
eq 'a stale checked head fails a pinned entry' 1 "$rc"
ok 'the checked entry records its typed mismatch' python3 -c 'import json,sys; e,=json.load(open(sys.argv[1])); assert e["status"]=="failed"; assert e["attempts"]==1; assert e["failure"]=={"expected":sys.argv[2],"found":sys.argv[3]}; assert "result" not in e' "$T/guarded.json" "$first_uuid" "$found"
no 'the stale checked run installs no source files' test -e ../guarded/second.txt
# An entry without an expectation keeps the existing merge behavior.
l queue add second --into main --no-run-queue --json > "$T/unpinned.json"
ok 'an unpinned entry omits the field' python3 -c 'import json,sys; assert "expected_head" not in json.load(open(sys.argv[1]))' "$T/unpinned.json"
ok 'an entry without an expectation merges' l queue run --into main
eq 'the unpinned entry lands' "$second" "$(l rev-parse head)"
no 'list refuses --expect-head' l queue list --expect-head "$base"
no 'add refuses --expect-state' l queue add second --expect-state "$second"
# A head move during checks overrides both a failing check and a later waiting check.
cat > "$T/move-head.sh" <<EOF
#!/bin/sh
set -e
export LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock
'$LAYR' --root '$LAYR_ROOT' --project p protect "\$1" --direct >/dev/null
'$LAYR' --root '$LAYR_ROOT' --project p --line "\$1" save --allow-empty -qm concurrent
'$LAYR' --root '$LAYR_ROOT' --project p protect "\$1" --no-direct >/dev/null
exit "\$2"
EOF
here=$(uname -s | tr '[:upper:]' '[:lower:]' | sed 's/^darwin$/macos/')-$(uname -m | sed 's/^arm64$/aarch64/')
other=linux-aarch64; [ "$here" != "$other" ] || other=macos-aarch64
l config check.wait true
l config checks.platforms "{\"wait\":\"$other\"}"
for outcome in waiting failed; do
    target=checks-$outcome
    l line new -q "$target" "$base"
    code=0; checks=move,wait
    if [ "$outcome" = failed ]; then code=7; checks=move; fi
    l config check.move "sh $T/move-head.sh $target $code"
    l protect "$target" --check "$checks" >/dev/null
    l queue add second --into "$target" --expect-head "$base" --no-run-queue >/dev/null
    rc=0
    l queue run --into "$target" --json > "$T/$outcome.json" || rc=$?
    eq "head movement takes precedence over $outcome checks" 1 "$rc"
    found=$(l line list --json | python3 -c 'import json,sys; print(next(e["head"] for e in json.load(sys.stdin) if e["name"]==sys.argv[1]))' "$target")
    ok "a $outcome check records the head mismatch without a retry" python3 -c 'import json,sys; e,=json.load(open(sys.argv[1])); assert e["status"]=="failed"; assert e["attempts"]==1; assert e["failure"]=={"expected":sys.argv[2],"found":sys.argv[3]}; assert sys.argv[2]!=sys.argv[3]; assert "result" not in e' "$T/$outcome.json" "$base_uuid" "$found"
    no "a $outcome check never installs the source" test -e "../$target/second.txt"
done
# Simulate a fetch's line-lock bypass after preparation, at the commit CAS.
for mode in normal checked; do
    target=commit-$mode
    l line new -q "$target" "$base"
    entry=$(l queue add second --into "$target" --expect-head "$base" --no-run-queue --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
    barrier=$T/$target
    mkdir "$barrier"
    args=()
    if [ "$mode" = checked ]; then args=(--checked-entry "$entry" --checked-head "$base" --checked-state "$second"); fi
    LAYR_TEST_COMMIT_BARRIER="$barrier" l queue run --into "$target" ${args[@]+"${args[@]}"} --json > "$T/$target.json" &
    job=$!
    trap 'touch "$barrier/release" 2>/dev/null || true; wait "$job" 2>/dev/null || true; cleanup' EXIT
    for attempt in $(seq 1 3000); do
        [ -f "$barrier/ready" ] && break
        kill -0 "$job" 2>/dev/null || break
        sleep 0.01
    done
    if [ ! -f "$barrier/ready" ]; then
        touch "$barrier/release"; wait "$job" || true
        echo 'FAIL queue did not reach the commit barrier'; exit 1
    fi
    # Change only the derived pointer to model a concurrent imported head; reindex restores it below.
    python3 - "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "$target" "$second_uuid" <<'PYMOVE'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.execute('UPDATE lines SET head=? WHERE name=?', (sys.argv[3], sys.argv[2]))
PYMOVE
    touch "$barrier/release"
    rc=0; wait "$job" || rc=$?
    trap cleanup EXIT
    eq "a late move fails the $mode runner" 1 "$rc"
    ok "the $mode runner records the commit-time mismatch without retrying" python3 -c 'import json,sys; e,=json.load(open(sys.argv[1])); assert e["status"]=="failed"; assert e["attempts"]==1; assert e["failure"]=={"expected":sys.argv[2],"found":sys.argv[3]}; assert "result" not in e' "$T/$target.json" "$base_uuid" "$second_uuid"
    no "the $mode commit race installs no source files" test -e "../$target/second.txt"
    l reindex -q
done
has 'queue records retain valid signatures' 'signatures ok' "$(l fsck)"
finish
