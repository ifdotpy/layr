#!/bin/bash
# ci: macos
# A merge worker can clone only main and still restore another state when it needs it.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper" >/dev/null 2>&1
l config check.gate true >/dev/null
l config checks.changes '["gate"]' >/dev/null
l config checks.lines '{"main":["gate"]}' >/dev/null
l protect main --check gate >/dev/null
for m in x y; do
  read id key <<<"$(env LAYR_ROOT="$T/$m/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"],d["public_key"])')"
  l remote trust cloud "$id" --public-key "$key" >/dev/null
done
l line new -q topic >/dev/null
echo topic > "$LAYR_ROOT/projects/p/lines/topic/only-topic.txt"
(cd "$LAYR_ROOT/projects/p/lines/topic" && l add only-topic.txt && l save -qm topic)
old=$(cd "$LAYR_ROOT/projects/p/lines/topic" && l log --json | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
echo newer > "$LAYR_ROOT/projects/p/lines/topic/only-topic.txt"
(cd "$LAYR_ROOT/projects/p/lines/topic" && l save -qam newer --amend)
topic=$(cd "$LAYR_ROOT/projects/p/lines/topic" && l log --json | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
l approve -q "$topic" >/dev/null
l push cloud -q >/dev/null 2>&1

xl() { env LAYR_ROOT="$T/x/store" "$LAYR" "$@"; }
xl clone "file://$T/archive" p --heads --only-line main >/dev/null 2>&1
eq "selected clone succeeds" 0 "$?"
ok "selected clone has main" test -d "$T/x/store/projects/p/lines/main"
no "selected clone does not materialize topic" test -e "$T/x/store/projects/p/lines/topic"
no "selected clone leaves topic state remote" test -d "$T/x/store/projects/p/states/$topic"
(cd "$T/x/store/projects/p/lines/main" && xl fetch origin --heads --only-line main >/dev/null 2>&1)
eq "selected fetch succeeds" 0 "$?"
no "selected fetch leaves topic state remote" test -d "$T/x/store/projects/p/states/$topic"
no "selected fetch requires --heads" env LAYR_ROOT="$T/x/store" sh -c 'cd "$1" && "$2" fetch origin --only-line main >/dev/null 2>&1' _ "$T/x/store/projects/p/lines/main" "$LAYR"
no "selected line is not a push option" env LAYR_ROOT="$T/x/store" sh -c 'cd "$1" && "$2" push origin --heads --only-line main >/dev/null 2>&1' _ "$T/x/store/projects/p/lines/main" "$LAYR"
pending=$(cd "$T/x/store/projects/p/lines/main" && xl checks pending --json)
ok "pending names the open change" python3 -c 'import json,sys; p=json.loads(sys.argv[1])["pending"]; assert any(x["state"]==sys.argv[2] and x["change"] is True for x in p)' "$pending" "$topic"
ok "pending distinguishes protected line checks" python3 -c 'import json,sys; p=json.loads(sys.argv[1])["pending"]; assert any(x["change"] is False for x in p)' "$pending"
ok "a superseded change is not marked open" python3 -c 'import json,sys; p=json.loads(sys.argv[1])["pending"]; assert not any(x["state"]==sys.argv[2] and x["change"] for x in p)' "$pending" "$old"
(cd "$T/x/store/projects/p/lines/main" && xl line new -q restored "$topic")
eq "another state restores on demand" 0 "$?"
ok "restored state has topic content" test -f "$T/x/store/projects/p/lines/restored/only-topic.txt"

yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
yl clone "file://$T/archive" p --heads --only-line missing >/dev/null 2>&1
no "an unknown line makes clone fail" test "$?" -eq 0
no "a failed clone removes the new project" test -e "$T/y/store/projects/p"
yl clone "file://$T/archive" p --only-line main >/dev/null 2>&1
no "a selected clone requires --heads" test "$?" -eq 0
yl clone "file://$T/archive" p --heads >/dev/null 2>&1
eq "an ordinary heads clone succeeds" 0 "$?"
ok "an ordinary heads clone materializes topic" test -f "$T/y/store/projects/p/lines/topic/only-topic.txt"
finish
