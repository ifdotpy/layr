#!/bin/bash
# ci: macos overlayfs
# The Layr Cloud merge machine as the change numbering machine (docs/review.md, "How numbers work"). The creator
# trusts a second machine with the merge role and hands it the numbering role. That machine runs only `queue run`:
# each pass numbers the changes without a number in one record, before it merges. After a fetch, the creator shows
# those numbers. A machine with a reader or writer role cannot get the numbering role. The merge machine cannot hand
# the role on; the creator, an admin machine, takes it back.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

js() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
number_of() { l change show "$1" --json | js "d['number']"; }
# The change.number records of machine $1 in the log of this machine.
numbers_by() { l op log --all --json | python3 -c "
import json,sys
print(sum(1 for line in sys.stdin if line.strip() and (r := json.loads(line)).get('op') == 'change.number' and r.get('machine') == '$1'))"; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
l line new -q first
cd "$P/first"
echo one > one.txt
l add one.txt
l save -q -m "feat: one"
C1=$(l log -n1 --json | head -1 | js "d['change_id']")
N1=$(number_of "$C1")
ok "the creator numbers its change" python3 -c "assert int('$N1') > 0"
ok "remote add" l remote add cloud "file://$T/plain"
ok "push to the store" l push cloud -q

FIRST=$LAYR_ROOT
new_fs "$T/merge" 2G
export LAYR_ROOT=$T/merge/store
cd "$T"
ok "the merge machine clones the store" l clone "file://$T/plain" p -q
M=$LAYR_ROOT/projects/p/lines
read -r MERGE_ID MERGE_KEY < <(cd "$M/main" && l remote self --json | js "d['id'] + ' ' + d['public_key']")
new_fs "$T/writer" 2G
export LAYR_ROOT=$T/writer/store
read -r WRITER_ID WRITER_KEY < <(l remote self --json | js "d['id'] + ' ' + d['public_key']")

export LAYR_ROOT=$FIRST
cd "$P/main"
CREATOR_ID=$(l remote self --json | js "d['id']")
ok "trust the merge machine" l remote trust cloud "$MERGE_ID" --public-key "$MERGE_KEY"
ok "narrow it to the merge role" l remote trust cloud "$MERGE_ID" --public-key "$MERGE_KEY" --role merge
ok "trust a cache writer" l remote trust cloud "$WRITER_ID" --public-key "$WRITER_KEY" --expires 1h --role writer
has "a cache writer cannot get the numbering role" "writer role" "$(l change numbering --hand-to "$WRITER_ID" 2>&1)"
ok "hand the numbering role to the merge machine" l change numbering --hand-to "$MERGE_ID"
eq "the creator no longer gives numbers" False "$(l change numbering --json | js "d['this_machine']")"
# Two changes after the handoff: one in the queue, one only pushed.
l line new -q queued
cd "$P/queued"
echo two > two.txt
l add two.txt
l save -q -m "feat: two"
C2=$(l log -n1 --json | head -1 | js "d['change_id']")
ok "queue the second change without a run" l queue add queued --into main --no-run-queue
cd "$P/main"
l line new -q pushed
cd "$P/pushed"
echo three > three.txt
l add three.txt
l save -q -m "feat: three"
C3=$(l log -n1 --json | head -1 | js "d['change_id']")
eq "the creator gives no number after the handoff" None "$(number_of "$C2")"
ok "push the handoff and the changes" l push cloud -q

# The merge machine runs only its pass: fetch, queue run, push.
export LAYR_ROOT=$T/merge/store
cd "$M/main"
ok "the merge machine fetches" l fetch origin -q
l queue run --into main --json >"$T/run1.out" 2>&1
eq "the pass merges the queued change" done "$(l queue list --into main --json | js "d[-1]['status']")" || cat "$T/run1.out"
eq "the pass wrote one number record" 1 "$(numbers_by "$MERGE_ID")"
l queue run --into main --json >"$T/run2.out" 2>&1
eq "a pass without new changes writes no number record" 1 "$(numbers_by "$MERGE_ID")"
has "a merge machine cannot hand the role on" "cannot hand the role on" "$(l change numbering --hand-to "$CREATOR_ID" 2>&1)"
ok "the merge machine pushes" l push origin -q

export LAYR_ROOT=$FIRST
cd "$P/main"
ok "the creator fetches" l fetch cloud -q
eq "the creator shows the merge machine as the numbering machine" "$MERGE_ID" "$(l change numbering --json | js "d['authority']")"
eq "the queued change has the next number" "$((N1 + 1))" "$(number_of "$C2")"
eq "the pushed change has a number too" "$((N1 + 2))" "$(number_of "$C3")"
eq "#$((N1 + 1)) selects the queued change" "$C2" "$(l change show "#$((N1 + 1))" --json | js "d['change_id']")"
eq "the first number stays" "$N1" "$(number_of "$C1")"

# The creator, an admin machine, takes the role back from the merge machine.
ok "the creator takes the role back" l change numbering --hand-to "$CREATOR_ID"
eq "the creator gives numbers again" True "$(l change numbering --json | js "d['this_machine']")"
l line new -q fourth
cd "$P/fourth"
echo four > four.txt
l add four.txt
l save -q -m "feat: four"
C4=$(l log -n1 --json | head -1 | js "d['change_id']")
eq "the creator numbers its new change next" "$((N1 + 3))" "$(number_of "$C4")"
ok "push the takeover" l push cloud -q
export LAYR_ROOT=$T/merge/store
cd "$M/main"
ok "the merge machine fetches the takeover" l fetch origin -q
eq "the merge machine no longer gives numbers" False "$(l change numbering --json | js "d['this_machine']")"
eq "the merge machine shows the creator's number" "$((N1 + 3))" "$(number_of "$C4")"
finish
