#!/bin/bash
# A checked queue run selects one exact entry and never runs a missing check.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
BASE=$(l rev-parse head)
l line new -q first
l line new -q second
cd ../first
echo first > first.txt; l add first.txt; l save -qm first
FIRST=$(l rev-parse head)
ENTRY=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../second
echo second > second.txt; l add second.txt; l save -qm second
SECOND=$(l rev-parse head)
SECOND_ENTRY=$(l queue add head -m 'fix: second' --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
CMD="if [ -n \"\${LAYR_CHECK_TMPDIR:-}\" ]; then echo run; else echo run >> $T/check-count; fi"
l config check.gate "$CMD"
l protect main --check gate
no 'all checked arguments are required' l queue run --into main --checked-entry "$ENTRY"
no 'an unknown checked entry is refused' l queue run --into main --checked-entry unknown --checked-head "$BASE" --checked-state "$FIRST"
no 'a changed expected head is refused' l queue run --into main --checked-entry "$ENTRY" --checked-head "$FIRST" --checked-state "$FIRST"
no 'a changed expected state is refused' l queue run --into main --checked-entry "$ENTRY" --checked-head "$BASE" --checked-state "$BASE"
ok 'a titled entry waits for checks of its resulting state' l queue run --into main --checked-entry "$SECOND_ENTRY" --checked-head "$BASE" --checked-state "$SECOND"
ok 'a missing check leaves the entry waiting' l queue run --into main --checked-entry "$ENTRY" --checked-head "$BASE" --checked-state "$FIRST"
eq 'the queue does not execute the missing check' no "$(test -e "$T/check-count" && echo yes || echo no)"
eq 'the target is unchanged' "$BASE" "$(l rev-parse head)"
l try -q "$FIRST" -- sh -c "$CMD"
ok 'a manual try does not satisfy the executor gate' l queue run --into main --checked-entry "$ENTRY" --checked-head "$BASE" --checked-state "$FIRST"
eq 'the manual result leaves the target unchanged' "$BASE" "$(l rev-parse head)"
if [ "$(id -u)" = 0 ]; then
  finish
  exit $?
fi
l config checks.changes '["gate"]' >/dev/null
l config --local checks.runner true >/dev/null
l approve -q "$FIRST" >/dev/null
ok 'the executor check passes' l checks run --state "$FIRST" --json
CHECKS_BEFORE=$(l attestations "$FIRST" --kind check --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
eq 'the manual and executor checks are recorded' 2 "$CHECKS_BEFORE"
ok 'the checked entry merges after its check passes' l queue run --into main --checked-entry "$ENTRY" --checked-head "$BASE" --checked-state "$FIRST"
eq 'the checked state is the new head' "$FIRST" "$(l rev-parse head)"
CHECKS_AFTER=$(l attestations "$FIRST" --kind check --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
eq 'the queue does not record another check' "$CHECKS_BEFORE" "$CHECKS_AFTER"
if [ "$(uname -s)" = Darwin ]; then
  eq 'only the manual try wrote outside the sandbox' 1 "$(wc -l < "$T/check-count" | xargs)"
else
  eq 'the check did not run again' 2 "$(wc -l < "$T/check-count" | xargs)"
fi
eq 'the other entry stays pending' pending "$(l queue list --json | python3 -c 'import json,sys; print(next(e["status"] for e in json.load(sys.stdin) if e["id"] == sys.argv[1]))' "$SECOND_ENTRY")"
no 'a completed checked entry is refused' l queue run --into main --checked-entry "$ENTRY" --checked-head "$FIRST" --checked-state "$FIRST"
finish
