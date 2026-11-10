#!/bin/bash
# ci: macos overlayfs
# Candidate preparation records an exact result without running checks or moving the target.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l line new -q topic
cd ../topic
echo source > source.txt; l add source.txt; l save -qm source
state=$(l rev-parse head)
entry=$(l queue add head -m 'fix: candidate' --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
echo target > target.txt; l add target.txt; l save -qm target
head=$(l rev-parse head)
l config check.gate "if [ -n \"\${LAYR_CHECK_TMPDIR:-}\" ]; then echo ran; else echo ran > $T/check-ran; fi" >/dev/null
l protect main --check gate >/dev/null
no 'candidate needs the expected target' l queue candidate "$entry" --expect-state "$state"
no 'a stale target is refused' l queue candidate "$entry" --expect-state "$state" --expect-head "$state"
no 'a different source is refused' l queue candidate "$entry" --expect-state "$head" --expect-head "$head"
out=$(l queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --json)
eq 'the reader does not create a missing candidate' None "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
out=$(l queue candidate "$entry" --expect-head "$head" --expect-state "$state" --json)
candidate=$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')
out=$(l queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --json)
eq 'the reader finds the prepared candidate' "$candidate" "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
ok 'the candidate has the source content' l show "$candidate:source.txt"
ok 'the candidate has the target content' l show "$candidate:target.txt"
eq 'the squash candidate has the requested title' 'fix: candidate' "$(sqlite3 "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "select json_extract(raw,'\$.r.states[0].message') from records where op='merge.candidate' order by time desc limit 1" | head -1)"
no 'candidate preparation does not run the check' test -e "$T/check-ran"
eq 'candidate preparation does not move main' "$head" "$(l rev-parse head)"
eq 'candidate preparation leaves the queue entry pending' pending "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
out=$(l queue candidate "$entry" --expect-head "$head" --expect-state "$state" --json)
eq 'the same input reuses the candidate' "$candidate" "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
l line new -q fast main
cd ../fast
echo fast > fast.txt; l add fast.txt; l save -qm fast
fast=$(l rev-parse head)
fast_entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
out=$(l queue candidate "$fast_entry" --expect-head "$head" --expect-state "$fast" --json)
fast_candidate=$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')
eq 'a fast-forward uses the source state as candidate' "$fast" "$(l rev-parse "$fast_candidate")"
eq 'the fast-forward candidate leaves main unchanged' "$head" "$(l rev-parse head)"
ok 'a different checked candidate leaves the entry waiting' l queue run --into main --checked-entry "$entry" --checked-head "$head" --checked-state "$state" --checked-candidate "$head"
eq 'a different candidate does not move main' "$head" "$(l rev-parse head)"
ok 'a candidate without checks leaves the entry waiting' l queue run --into main --checked-entry "$entry" --checked-head "$head" --checked-state "$state" --checked-candidate "$candidate"
eq 'the candidate without checks does not move main' "$head" "$(l rev-parse head)"
l try -q "$candidate" -- sh -c "echo ran > $T/check-ran"
ok 'a manual try cannot approve a queue candidate' l queue run --into main --checked-entry "$entry" --checked-head "$head" --checked-state "$state" --checked-candidate "$candidate"
eq 'the manual try leaves main unchanged' "$head" "$(l rev-parse head)"
if [ "$(id -u)" = 0 ]; then
  finish
  exit $?
fi
l config checks.changes '["gate"]' >/dev/null
l config --local checks.runner true >/dev/null
l approve -q "$candidate" >/dev/null
ok 'the executor checks the candidate' l checks run --state "$candidate" --json
ok 'the checked squash candidate merges' l queue run --into main --checked-entry "$entry" --checked-head "$head" --checked-state "$state" --checked-candidate "$candidate"
eq 'main takes exactly the checked candidate' "$(l rev-parse "$candidate")" "$(l rev-parse head)"
l protect main --remove >/dev/null
head=$(l rev-parse head)
out=$(l queue candidate "$fast_entry" --expect-head "$head" --expect-state "$fast" --json)
candidate=$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')
ok 'a checked candidate also merges when no checks are configured' l queue run --into main --checked-entry "$fast_entry" --checked-head "$head" --checked-state "$fast" --checked-candidate "$candidate"
eq 'the candidate without configured checks is the new head' "$(l rev-parse "$candidate")" "$(l rev-parse head)"
finish
