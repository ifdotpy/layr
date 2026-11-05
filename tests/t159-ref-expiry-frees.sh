#!/bin/bash
# ci: macos
# A ref with a time to live (docs/ownership.md): when its time runs out, the next command expires it and releases
# the state it kept, with no `gc`: a state that nothing else keeps is freed in the same command.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l config undo.ops 1 >/dev/null
base=$(l rev-parse head)
echo one > one.txt; l add one.txt >/dev/null; l save -qm one
kept=$(l rev-parse head)
kept_id=$(l log --json -n 1 2>/dev/null | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["id"])')
ref_owners() { sqlite3 "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "select count(*) from owners where state='$kept_id' and kind='ref'"; }
l ref set holds/short "$kept" --ttl 1s >/dev/null
l reset --hard "$base" -q >/dev/null 2>&1
echo two > two.txt; l add two.txt >/dev/null; l save -qm two
eq "the ref keeps the state while it lives" 1 "$(ref_owners)"
sleep 2
l status >/dev/null 2>&1
no "the next command after its time expires the ref" grep -q holds/short <<<"$(l ref list 2>&1)"
eq "and releases the state it kept, so the free step can free it" 0 "$(ref_owners)"
finish
