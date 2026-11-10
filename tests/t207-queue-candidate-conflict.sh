#!/bin/bash
# ci: macos overlayfs
# A conflict is recorded on the queue entry without moving the target.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l line new -q topic
cd ../topic
echo source > notes.txt; l save -qam source
state=$(l rev-parse head)
entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
echo target > notes.txt; l save -qam target
head=$(l rev-parse head)
no 'a stale head cannot fail the entry' l queue candidate "$entry" --expect-head "$state" --expect-state "$state" --json
eq 'the stale request leaves it pending' pending "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
out=$(l queue candidate "$entry" --expect-head "$head" --expect-state "$state" --json 2>&1); code=$?
eq 'a conflicting candidate returns a failure' 1 "$code"
has 'the response contains the conflicting path' notes.txt "$out"
eq 'the queue entry records the failure' failed "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
has 'the queue entry exposes the conflict reason' notes.txt "$(l queue list --json)"
eq 'the conflicting candidate does not move main' "$head" "$(l rev-parse head)"
finish
