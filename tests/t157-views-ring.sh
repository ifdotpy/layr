#!/bin/bash
# ci: macos
# Views of thin states (docs/ownership.md): a view is a complete copy of a thin state's layers that a read made.
# When a command makes a new view, only the newest `thin.views` stay, with no `gc`; the others go to the trash and
# a later read makes them again.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l config thin.views 2 >/dev/null
l config thin.view_seconds 0 >/dev/null
ids=()
for i in 1 2 3 4 5; do
  echo $i > a$i.txt; l add a$i.txt >/dev/null; l save -qm a$i
  ids+=("$(l log --json -n 1 2>/dev/null | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["id"])')")
done
echo 6 > a6.txt; l add a6.txt >/dev/null; l save -qm a6
views() { ls "$LAYR_ROOT/projects/p/meta/views" 2>/dev/null | wc -l | tr -d ' '; }
n=0
for id in "${ids[@]}"; do n=$((n + 1)); l line new w$n "$id" >/dev/null 2>&1; done
ok "each new line from an older state started" test -f "$LAYR_ROOT/projects/p/lines/w1/a1.txt"
ok "only the newest views stay" test "$(views)" -le 2
l line new again "${ids[0]}" >/dev/null 2>&1
eq "a read makes a view again" 1 "$(cat "$LAYR_ROOT/projects/p/lines/again/a1.txt" 2>&1)"
finish
