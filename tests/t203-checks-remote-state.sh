#!/bin/bash
# ci: macos overlayfs
# An explicit check restores its source after a heads-only clone.
. "$(dirname "$0")/lib.sh"
needs_nested_sandbox
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l config check.gate true >/dev/null
l config checks.changes '["gate"]' >/dev/null
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
xl() { env LAYR_ROOT="$T/x/store" "$LAYR" "$@"; }
read id key <<<"$(xl remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$id" --public-key "$key" >/dev/null
echo first > first.txt
l add first.txt >/dev/null
l save -qm first
first=$(l log --json | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
l approve -q "$first" >/dev/null
echo second > second.txt
l add second.txt >/dev/null
l save -qm second
second=$(l log --json | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
l approve -q "$second" >/dev/null
echo third > third.txt
l add third.txt >/dev/null
l save -qm third
l push cloud -q >/dev/null 2>&1
xl clone "file://$T/archive" p --heads >/dev/null 2>&1
cd "$T/x/store/projects/p/lines/main"
xl config --local checks.runner true >/dev/null
no 'the older state is absent after a heads-only clone' test -d "$T/x/store/projects/p/states/$first"
out=$(xl checks run --state "$first" --json 2>&1)
has 'the explicit check restores and runs the older state' '"count":1' "$out"
out=$(xl checks pending --state "$first" --passing --required-checks gate --json 2>&1)
has 'the passing result satisfies the gate' '"pending":[]' "$out"
mv "$T/archive" "$T/archive-offline"
no 'an explicit check fails when its source cannot be restored' xl checks run --state "$second" --json
out=$(xl checks pending --state "$second" --passing --required-checks gate --json)
has 'the unavailable source has no passing result' '"name":"gate"' "$out"
finish
