#!/bin/bash
# ci: macos
# A thin line head gets a complete tree after the save that makes its chain longer than `thin.chain`
# (docs/ownership.md): a background process makes it, with no `gc`, so no chain grows without end and the
# bases below it can go.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l config thin.chain 2 >/dev/null
head() { l log --json -n 1 2>/dev/null | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["id"])'; }
full() { local h; h=$(head); test -n "$h" && test -d "$LAYR_ROOT/projects/p/states/$h"; }
echo 1 > a1.txt; l add a1.txt >/dev/null; l save -qm a1
no "a short chain stays thin" full
for i in 2 3 4; do echo $i > a$i.txt; l add a$i.txt >/dev/null; l save -qm a$i; done
for _ in $(seq 1 50); do full && break; sleep 0.2; done
ok "the save that makes the chain too long gives the head a complete tree" full
eq "the tree has the files" 4 "$(ls "$LAYR_ROOT/projects/p/states/$(head)" | grep -c '^a[0-9].txt$')"
finish
