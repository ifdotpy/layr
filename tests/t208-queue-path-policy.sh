#!/bin/bash
# ci: macos overlayfs
# A privileged queue must not skip the creator's per-path protection.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
echo base > "$T/src/secret.txt"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l line new -q protected
l line new -q ordinary
cd ../protected
echo change > secret.txt; l save -qam protected
protected=$(l rev-parse head)
entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../ordinary
echo ordinary > ordinary.txt; l add ordinary.txt; l save -qm ordinary
ordinary=$(l rev-parse head)
ordinary_entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
head=$(l rev-parse head)
l protect main --path secret.txt=admin >/dev/null
ok 'an authorized local admin can prepare the protected change' l queue candidate "$entry" --expect-head "$head" --expect-state "$protected" --json
eq 'the protected entry remains pending until checks pass' pending "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
ok 'an unrelated change can still prepare a candidate' l queue candidate "$ordinary_entry" --expect-head "$head" --expect-state "$ordinary" --json
eq 'candidate preparation does not move main' "$head" "$(l rev-parse head)"
finish
