#!/bin/bash
# ci: macos
# A heads-only clone must reuse the queued merge candidate on the remote.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
owner=$(l remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
xl() { env LAYR_ROOT="$T/x/store" "$LAYR" "$@"; }
read id key <<<"$(xl remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$id" --public-key "$key" >/dev/null
base=$(l rev-parse head)
l line new -q topic
cd ../topic
echo one > one.txt; l add one.txt; l save -qm one
cd ../main
echo two > two.txt; l add two.txt; l save -qm two
l config check.gate 'true' >/dev/null
l config checks.platforms '{"gate":"linux-x86_64"}' >/dev/null
l protect main --check gate >/dev/null
entry=$(l queue add topic --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
head=$(l rev-parse head)
state=$(l rev-parse topic)
l queue run --into main >/dev/null 2>&1 || true
out=$(l queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --json)
eq 'a legacy candidate is not treated as safely prepared' None "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
l queue candidate "$entry" --expect-head "$head" --expect-state "$state" >/dev/null
first=$(sqlite3 "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "select json_extract(raw,'\$.r.states[0].id') from records where op='merge.candidate' order by time desc limit 1")
ok 'the first pass records a candidate' test -n "$first"
l push cloud -q >/dev/null 2>&1
xl clone "file://$T/archive" p --heads >/dev/null 2>&1
cd "$T/x/store/projects/p/lines/main"
no 'the candidate is absent from the heads-only clone' test -d "$T/x/store/projects/p/states/$first"
out=$(xl queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --json)
eq 'the reader finds the remote candidate by metadata' "$first" "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
out=$(xl queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --candidate-machine "$owner" --json)
eq 'the reader finds the candidate of the merge machine' "$first" "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
out=$(xl queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --candidate-machine "$id" --json)
eq 'the reader ignores a candidate of another machine' None "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["candidate"])')"
no 'the machine filter requires a full UUID' xl queue candidate "$entry" --expect-head "$head" --expect-state "$state" --existing --candidate-machine invalid
no 'the reader does not restore the candidate tree' test -d "$T/x/store/projects/p/states/$first"
xl config check.gate 'true' >/dev/null
xl config checks.platforms '{"gate":"linux-x86_64"}' >/dev/null
xl protect main --check gate >/dev/null
out=$(xl queue run --into main --checked-entry "$entry" --checked-head "$head" --checked-state "$state" --checked-candidate "$first" 2>&1)
has 'a guarded merge does not trust another machine candidate' 'checked merge candidate is unavailable' "$out"
xl show "$base:notes.txt" >/dev/null
mv "$T/archive" "$T/archive-offline"
out=$(xl queue run --into main 2>&1)
has 'an unavailable candidate leaves the entry waiting' 'cannot restore merge candidate' "$out"
eq 'an unavailable candidate does not fail the entry' pending "$(xl queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
mv "$T/archive-offline" "$T/archive"
out=$(xl queue run --into main 2>&1)
has 'the next pass waits for the original candidate check' 'has no passing run on linux-x86_64' "$out"
count=$(sqlite3 "$T/x/store/projects/p/meta/db/layr.sqlite" "select count(*) from records where op='merge.candidate'")
eq 'the next pass reuses the original candidate' 2 "$count"
xl protect main --remove >/dev/null
echo three > three.txt; xl add three.txt >/dev/null; xl save -qm three
xl protect main --check gate >/dev/null
xl queue run --into main >/dev/null 2>&1 || true
count=$(sqlite3 "$T/x/store/projects/p/meta/db/layr.sqlite" "select count(*) from records where op='merge.candidate'")
eq 'a changed target creates a new candidate' 3 "$count"
finish
