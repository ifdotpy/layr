#!/bin/bash
# A failed executor result must remain pending for a merge gate.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l config check.fail false >/dev/null
l config check.pass true >/dev/null
l config checks.changes '["fail","pass"]' >/dev/null
l config --local checks.runner true >/dev/null
no 'a failed executor makes checks run fail' l checks run --state main --json
count() { python3 -c 'import json,sys; print(" ".join(sorted(p["name"] for p in json.load(sys.stdin)["pending"])))'; }
eq 'the executor does not retry completed failures by default' '' "$(l checks pending --state main --json | count)"
l config checks.changes '[]' >/dev/null
out=$(l checks pending --state main --passing --required-checks fail,pass --json)
eq 'the merge gate waits for the failed result' fail "$(printf '%s' "$out" | count)"
ok 'the gate returns creator commands for both required checks' python3 -c 'import json,sys; x=json.loads(sys.argv[1])["required_definitions"]; assert x["fail"]["command"] == "false" and x["pass"]["command"] == "true"' "$out"
no 'the gate refuses an undefined required check' l checks pending --state main --passing --required-checks missing --json
echo next > next.txt
l add next.txt >/dev/null
l save -qm next
out=$(l checks run --state main --required-checks pass --json 2>&1)
has 'an explicit run executes a required check outside checks.changes' '"count":1' "$out"
eq 'the required check now has a passing result' '' "$(l checks pending --state main --passing --required-checks pass --json | count)"
no 'an explicit run refuses an undefined required check' l checks run --state main --required-checks missing --json
finish
