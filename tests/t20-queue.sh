#!/bin/bash
# Exact queue entries, atomic completion, failures, target CAS, and daemon passes.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
BASE=$(l rev-parse head)
l line new -q topic
cd ../topic
echo one > one.txt; l add one.txt; l save -qm one
ONE=$(l rev-parse head)
ADDED=$(l queue add topic --json)
Q=$(printf '%s' "$ADDED" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
QUEUED_STATE=$(printf '%s' "$ADDED" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')
# A line name that equals the stored UUID must not redirect the queue source.
l line new -q "$QUEUED_STATE" main
echo two > two.txt; l add two.txt; l save -qm two
cd ../main
ok 'the exact queued state merges' l queue run --into main
eq 'a line name cannot redirect the exact queued state' "$ONE" "$(l rev-parse head)"
eq 'a later source commit stays out' no "$(test -e two.txt && echo yes || echo no)"
eq 'completion status is done' done "$(l queue list --into main --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
ok 'undo reverts queue completion and the line together' l undo
eq 'undo restores the old head' "$BASE" "$(l rev-parse head)"
eq 'undo restores pending status' pending "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
ok 'a pending entry runs again' l queue run --into main
l line new -q destination
l config line.topic.upstream destination
cd ../topic
UP=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
eq 'the upstream selects the target' destination "$(l queue list --into destination --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["into"])')"
ok 'remove deletes an entry' l queue remove "$UP"
eq 'the removed entry is absent' 0 "$(l queue list --into destination --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ok 'undo restores a removed entry' l undo
l queue remove "$UP" >/dev/null
cd ../main
# A failed conflict must not install conflict content on an unprotected line.
l line new -q plain
l line new -q conflict-source plain
cd ../conflict-source
echo source > notes.txt; l save -qam source
cd ../plain
echo target > notes.txt; l save -qam target
PLAIN=$(l rev-parse head)
BAD=$(l queue add conflict-source --into plain --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
no 'a conflict fails the queue entry' l queue run --into plain
eq 'a conflict leaves the target head' "$PLAIN" "$(l rev-parse head)"
eq 'a conflict leaves the target content' target "$(cat notes.txt)"
has 'a conflict reason names its path' notes.txt "$(l queue list --into plain)"
l undo >/dev/null
l line new -q good-source plain
cd ../good-source
echo good > good.txt; l add good.txt; l save -qm good
GOOD=$(l rev-parse head)
l queue add head --into plain >/dev/null
cd ../plain
no 'the queue reports a failed entry and continues' l queue run --into plain
eq 'the following good entry merges' "$GOOD" "$(l rev-parse head)"
eq 'both results are recorded' 'failed done' "$(l queue list --into plain --json | python3 -c 'import json,sys; print(" ".join(x["status"] for x in json.load(sys.stdin)))')"
# The queue uses checks on the result and reuses a passing exact-state result.
cd ../main
l line new -q checked-source
cd ../checked-source
echo checked > checked.txt; l add checked.txt; l save -qm checked
CHECKED=$(l rev-parse head)
cd ../main
BEFORE=$(l rev-parse head)
l protect main --check gate
l config check.gate '"false"'
l queue add checked-source >/dev/null
no 'a failed check fails the entry' l queue run --into main
eq 'a failed check leaves the head' "$BEFORE" "$(l rev-parse head)"
has 'the reason names the failed check' "check 'gate' failed" "$(l queue list --into main)"
CMD="echo checked >> $T/check-count"
l config check.gate "$CMD"
l try -q "$CHECKED" -- sh -c "$CMD"
l queue add "$CHECKED" >/dev/null
ok 'a passing check can be reused' l queue run --into main
eq 'the reused check runs only once' 1 "$(wc -l < "$T/check-count" | xargs)"
# A check changes the target while the merge lock is released. Retry exactly once.
l line new -q moved-source
cd ../moved-source
echo moved > moved.txt; l add moved.txt; l save -qm moved
cd ../main
cat > "$T/move-head" <<EOF
#!/bin/sh
if '$LAYR' --root '$LAYR_ROOT' --project p queue run >/dev/null 2>&1; then exit 1; fi
if [ ! -e '$T/moved-once' ]; then
  touch '$T/moved-once'
  '$LAYR' --root '$LAYR_ROOT' --project p --line main save --allow-empty -qm concurrent
fi
EOF
chmod +x "$T/move-head"
l config check.gate "$T/move-head"
MOVE=$(l queue add moved-source --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
ok 'a moved target retries and then merges' l queue run --into main
eq 'the entry makes two attempts' 2 "$(l queue list --json | python3 -c 'import json,sys; print(next(e["attempts"] for e in json.load(sys.stdin) if e["id"]==sys.argv[1]))' "$MOVE")"
has 'the concurrent head stays in history' concurrent "$(l log --format=%s)"
ok 'the queued content reaches the target' test -f moved.txt
# Continuous head changes fail after the second attempt.
l line new -q churn-source
cd ../churn-source
echo churn > churn.txt; l add churn.txt; l save -qm churn
cd ../main
cat > "$T/churn-head" <<EOF
#!/bin/sh
'$LAYR' --root '$LAYR_ROOT' --project p --line main save --allow-empty -qm concurrent-again
EOF
chmod +x "$T/churn-head"
l config check.gate "$T/churn-head"
CHURN=$(l queue add churn-source --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
no 'a second moved head fails the entry' l queue run --into main
eq 'a moved head has at most two attempts' 'failed 2' "$(l queue list --json | python3 -c 'import json,sys; e=next(e for e in json.load(sys.stdin) if e["id"]==sys.argv[1]); print(e["status"],e["attempts"])' "$CHURN")"
no 'a failed CAS does not install the queued file' test -e churn.txt
# Pending entries keep an otherwise unreachable source through their hold ref.
l line new -q held-source
cd ../held-source
echo held > held.txt; l add held.txt; l save -qm held
HELD=$(l rev-parse head)
HOLD=$(l queue add head --into destination --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
l line delete -D held-source >/dev/null
l config undo.days 0; l config retention.auto 0; l config retention.hours 0
ok 'GC keeps a pending queue source' l gc
ok 'the held source stays readable' l show "$HELD":held.txt
l queue remove "$HOLD" >/dev/null
l gc >/dev/null
no 'removal releases an unreachable source' l show "$HELD":held.txt
l config undo.days 7
# Enabled queues run after add, and existing pending entries run on a daemon tick.
l line new -q automatic
l line new -q automatic-source automatic
cd ../automatic-source
echo automatic > automatic.txt; l add automatic.txt; l save -qm automatic
AUTO=$(l rev-parse head)
l config queue.run automatic
ADDED=$(l queue add head --into automatic --json)
eq 'queue.run processes a new entry' done "$(printf '%s' "$ADDED" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"
eq 'automatic run moves the target' "$AUTO" "$(l --line automatic rev-parse head)"
l config --unset queue.run
echo tick > tick.txt; l add tick.txt; l save -qm tick
TICK=$(l rev-parse head)
l queue add head --into automatic >/dev/null
l config queue.run automatic
"$LAYR" daemon --socket "$T/queue.sock" --tick 1 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 150); do [ "$(l --line automatic rev-parse head)" = "$TICK" ] && break; sleep 0.1; done
eq 'a daemon tick processes the pending entry' "$TICK" "$(l --line automatic rev-parse head)"
ok 'the queue index rebuilds' l reindex -q
has 'queue records have valid signatures' 'signatures ok' "$(l fsck)"
finish
