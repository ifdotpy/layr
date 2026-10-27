#!/bin/bash
# Real remote/clone caller. Test paper key stays only in the Mac owner's temporary folder.
set -euo pipefail
test "$(uname -s)" = Darwin
test "$(id -u)" != 0
L=${LAYR:?set LAYR to the binary under test}
base=$(cd "$(mktemp -d)" && pwd -P)
trap 'chflags -R nouchg "$base" 2>/dev/null || true; chmod -R u+w "$base" 2>/dev/null || true; rm -rf "$base"' EXIT
unset LAYR_SOCKET LAYR_PROJECT LAYR_LINE
export LAYR_ROOT="$base/runner"
"$L" remote self --json > "$base/runner.json"
runner_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$base/runner.json")
runner_key=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["public_key"])' "$base/runner.json")
export LAYR_ROOT="$base/owner"
mkdir "$base/source"
echo initial > "$base/source/value"
"$L" init p --from "$base/source" -q
cd "$LAYR_ROOT/projects/p/lines/main"
"$L" config check.smoke 'echo creator-command'
"$L" config checks.changes '["smoke"]'
"$L" config checks.lines '{"main":["smoke"]}'
"$L" remote add origin "file://$base/remote" --paper-key-out "$base/test-paper-key" >/dev/null
"$L" remote trust origin "$runner_id" --public-key "$runner_key" >/dev/null
"$L" line new topic -q
cd ../topic
echo changed > value
"$L" save -qam changed
"$L" approve head -q
"$L" push origin -q
export LAYR_ROOT="$base/runner"
cd "$base"
"$L" clone "file://$base/remote" p -q
cd "$LAYR_ROOT/projects/p/lines/main"
"$L" config access | python3 -c 'import json,os,sys; a=json.load(sys.stdin); assert a["*"]=="writer" and a["user:"+str(os.getuid())]=="admin",a'
"$L" fetch origin -q
"$L" config check.smoke 'echo runner-override; exit 99'
"$L" config check.local-only 'exit 98'
"$L" config checks.changes '["local-only"]'
"$L" config checks.lines '{"main":["local-only"]}'
"$L" protect main --remove
"$L" config --local checks.runner true
"$L" checks watch --once --json > "$base/results.json"
python3 - "$base/results.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r['count']==2 and all(c['name']=='smoke' and c['command']=='echo creator-command' and c['exit']==0 for c in r['checks']),r
PY
"$L" fsck
echo 'PASS: clone can fetch; creator checks and protection ignore runner-local overrides'
