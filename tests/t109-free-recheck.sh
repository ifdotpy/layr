#!/bin/bash
# ci: macos overlayfs
# The free step checks and moves a queued state under the project lock (docs/ownership.md, "When a
# state is freed"). A state queued for freeing that another command re-owns before the free step
# keeps its storage. Test hook: LAYR_TEST_FREE_PAUSE_MS pauses the free step of the first command
# before its lock, so the second command re-owns the state first (test only).
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p
DB=$P/meta/db/layr.sqlite
[ -f "$DB" ] || DB=$P/meta/layr.sqlite
M=$P/lines
q() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute(sys.argv[2]).fetchone()[0])' "$DB" "$1"; }
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
on_disk() { [ -e "$P/states/$(dashed "$1")" ] || [ -e "$P/deltas/$(dashed "$1")" ]; }

cd $M/main || exit 1
l config undo.ops 0 >/dev/null
l config undo.deleted 0 >/dev/null
l config retention.auto 0 >/dev/null
# A line whose head is kept by a ref only: its line delete leaves the head with that ref as owner.
l line new -q q >/dev/null
cd $M/q || exit 1
echo q >> notes.txt; l save -qam q >/dev/null
S=$(l rev-parse head)
cd $M/main || exit 1
l ref set pins/s "$S" >/dev/null
l line delete -D q -q >/dev/null
ok "the state is kept by its ref" on_disk "$S"

# The first command drops the ref: the state is queued, and its free step pauses before the lock.
( env LAYR_TEST_FREE_PAUSE_MS=4000 "$LAYR" ref delete pins/s > "$T/del.out" 2>&1; echo $? > "$T/del.rc" ) &
sleep 1.5
# The second command names the state again while the first one pauses.
l ref set pins/s "$S" >/dev/null
wait
eq "the ref delete finishes" 0 "$(cat "$T/del.rc")"
ok "a re-owned state keeps its storage after the free step" on_disk "$S"
eq "the re-owned state is present again" 1 "$(q "select present from states where display='$S'")"
has "the re-owned state reads its content" "q" "$(l show "$S:notes.txt" | tail -1)"
eq "the free queue is empty" 0 "$(q "select count(*) from free_queue")"
finish
