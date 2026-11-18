#!/bin/bash
# After a queued merge, the entries behind it that conflict with the new head fail at once.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
for name in first second third; do l line new -q "$name"; done
cd ../first
echo first > notes.txt; l save -qam first
cd ../second
echo second > notes.txt; l save -qam second
cd ../third
echo third > third.txt; l add third.txt; l save -qm third
cd ../main
for name in first second third; do
    eval "$name=\$(l queue add \"\$name\" --no-run-queue --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"id\"])')"
done
status() { l queue list --json | python3 -c "import json,sys; e=[x for x in json.load(sys.stdin) if x['id']==sys.argv[1]][0]; print(e['status'], e.get('attempts'))" "$1"; }
no 'the queue reports the failed entry' l queue run --into main
eq 'the conflict pass fails the second entry before its turn' 'failed 0' "$(status "$second")"
has 'its reason names the path' 'queued merge has conflicts: notes.txt' "$(l queue list)"
eq 'an entry without a conflict still merges' done "$(status "$third" | cut -d' ' -f1)"
eq 'main has the first and the third entry' "first yes" "$(cat notes.txt) $(test -e third.txt && echo yes)"
finish
