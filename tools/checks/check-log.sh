#!/bin/bash
# The check results of a state and their logs, from the records of the project: a runner pushes each log as a
# derived state, so a log is readable from the cloud while its runner sleeps (no SSM, no runner store).
#   tools/checks/check-log.sh <rev>                 list the checks of <rev>: platform, check, exit, log state
#   tools/checks/check-log.sh <rev> <check>         print the newest log of that check (all platforms)
#   tools/checks/check-log.sh <rev> --failed        print the newest log of each failed check
# Fetch the records first (`layr fetch <remote> --heads`) when the results are newer than this store.
set -euo pipefail
rev=${1:?usage: check-log.sh <rev> [<check>|--failed]}
want=${2:-}
state=$(layr log -n 1 --json "$rev" | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["id"])')
results=$(layr op export | python3 -c '
import json, sys
state = sys.argv[1]
newest = {}
for line in sys.stdin:
    record = json.loads(line)["r"]
    check = (record.get("data") or {}).get("check")
    if record.get("op") != "attest" or not isinstance(check, dict) or check.get("state") != state:
        continue
    key = (check.get("platform", "?"), check.get("name", "?"))
    if key not in newest or record["time"] >= newest[key][0]:
        newest[key] = (record["time"], check.get("exit"), check.get("output_state", ""))
for (platform, name), (_, code, output) in sorted(newest.items()):
    print(platform, name, code, output)
' "$state")
if [ -z "$want" ]; then
    printf '%s\n' "$results" | awk 'NF { printf "%-16s %-16s exit %-4s %s\n", $1, $2, $3, $4 }'
    exit 0
fi
printf '%s\n' "$results" | while read -r platform name code output; do
    if [ "$want" = "$name" ] || { [ "$want" = --failed ] && [ "$code" != 0 ]; }; then
        echo "===== $name on $platform: exit $code ($output)"
        [ -n "$output" ] && layr show "$output:check.log"
    fi
done
