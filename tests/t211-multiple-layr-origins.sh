#!/bin/bash
# ci: macos overlayfs
# Two Layr origins report a partial failure and accept a safe retry.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init p --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l remote add first "file://$T/first" --paper-key-out "$T/first-paper" >/dev/null
l remote add second "file://$T/second" --paper-key-out "$T/second-paper" >/dev/null
echo change > change.txt
l add change.txt
l save -qm change
mv "$T/second" "$T/second-away"
touch "$T/second"
set +e
result=$(l push first second --json 2>"$T/push-error")
code=$?
set -e
ok 'a required origin failure returns an error' test "$code" -ne 0
ok 'the report identifies both the successful and failed origins' python3 -c '
import json, sys
rows = {r["name"]: r for r in json.loads(sys.argv[1])["remotes"]}
assert rows["first"]["pushed"] == ["*"] and not rows["first"]["failed"], rows
assert rows["second"]["failed"] and not rows["second"]["pushed"], rows
' "$result"
rm "$T/second"
mv "$T/second-away" "$T/second"
result=$(l push first second --json)
ok 'retry succeeds for both origins' python3 -c '
import json, sys
rows = json.loads(sys.argv[1])["remotes"]
assert {r["name"] for r in rows} == {"first", "second"}, rows
assert all(r["pushed"] == ["*"] and not r["failed"] for r in rows), rows
' "$result"
# The retry records its successful transfers locally. Send those records before
# measuring an idle push; they do not create new source states.
result=$(l push first second --json)
ok 'the next push sends no duplicate states' python3 -c '
import json, sys
rows = json.loads(sys.argv[1])["remotes"]
assert all(r["report"]["states"] == 0 for r in rows), rows
' "$result"
first_count=$(find "$T/first/records" -type f | wc -l | tr -d ' ')
second_count=$(find "$T/second/records" -type f | wc -l | tr -d ' ')
l push first second -q
eq 'repeating the push does not add records to the first origin' "$first_count" "$(find "$T/first/records" -type f | wc -l | tr -d ' ')"
eq 'repeating the push does not add records to the second origin' "$second_count" "$(find "$T/second/records" -type f | wc -l | tr -d ' ')"
finish
