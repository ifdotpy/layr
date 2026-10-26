#!/bin/bash
# Run unprivileged on macOS with the binary under test. Real watcher, gate, logs and signatures.
set -euo pipefail
test "$(uname -s)" = Darwin
test "$(id -u)" != 0
LAYR=${LAYR:?set LAYR to the absolute binary under test}
task_tmp=$(cd "$(mktemp -d)" && pwd -P)
cleanup() {
    result=$?
    if [ "$result" != 0 ]; then echo "Failed watcher evidence retained at $task_tmp" >&2; return; fi
    chflags -R nouchg "$task_tmp" 2>/dev/null || true
    chmod -R u+w "$task_tmp" 2>/dev/null || true
    rm -rf "$task_tmp"
}
trap cleanup EXIT
export LAYR_ROOT="$task_tmp/store"
unset LAYR_SOCKET LAYR_PROJECT LAYR_LINE
mkdir "$task_tmp/source"
echo initial > "$task_tmp/source/value"
"$LAYR" init p --from "$task_tmp/source" -q
cd "$LAYR_ROOT/projects/p/lines/main"
l() { "$LAYR" "$@"; }
l config check.smoke 'echo sandbox-check; test "$(id -u)" != 0'
l config checks.lines '{"main":["smoke"]}'
l config checks.changes '["smoke"]'
l config --local checks.runner true
if l config --local checks.privileged-runner true; then echo 'Mac privileged opt-in accepted' >&2; exit 1; fi
l checks watch --once --json > "$task_tmp/main.json"
python3 - "$task_tmp/main.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']==1 and r['checks'][0]['exit']==0, r
PY
l line new topic -q
cd ../topic
echo changed > value
l save -qam changed
state=$(l rev-parse head)
l checks watch --once --json > "$task_tmp/unapproved.json"
python3 - "$task_tmp/unapproved.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']==0 and r['pending']==1, r
PY
l approve "$state" -q
l checks watch --once --json > "$task_tmp/approved.json"
python3 - "$task_tmp/approved.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']==1 and r['checks'][0]['exit']==0, r
PY
# Approval does not authorize an amended version. Its old approval cannot execute it.
echo amended > value
l save -qam amended --amend
l checks watch --once --json > "$task_tmp/amended.json"
python3 - "$task_tmp/amended.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']==0 and r['pending']==1, r
PY
l approve head -q
l approve head --withdraw -q
l checks watch --once --json > "$task_tmp/withdrawn.json"
python3 - "$task_tmp/withdrawn.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']==0 and r['pending']==1, r
PY
l approve head -q
l config check.smoke 'echo retained-failure; exit 7'
rc=0
l checks watch --once --json > "$task_tmp/failed.json" || rc=$?
test "$rc" = 1
python3 - "$task_tmp/failed.json" > "$task_tmp/log-ref" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']>0 and all(c['exit']==7 for c in r['checks']), r
print(r['checks'][-1]['log_ref'])
PY
l show "$(cat "$task_tmp/log-ref"):check.log" | grep retained-failure
l fsck
echo 'PASS: macOS main, approval, exact version, withdrawal, failed attestation and retained log'
