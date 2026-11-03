#!/bin/bash
# Pins (docs/ownership.md, "Pins"): a command that reads saved states keeps them on disk until it
# ends, though another command frees their last owner meanwhile. Test hook: LAYR_TEST_PIN_HOLD_MS
# makes the reader sleep once its pins are taken (test only). Each case: the reader holds its pins;
# a second process drops the last owner of a state the reader reads; the reader finishes with the
# right result; the state goes after the reader ends. Three readers: a merge source, the base of a
# prepared save, and an overlap compare of status.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p
M=$P/lines
cd $M/main || exit 1
DB=$P/meta/db/layr.sqlite
[ -f "$DB" ] || DB=$P/meta/layr.sqlite
q() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute(sys.argv[2]).fetchone()[0])' "$DB" "$1"; }
# A state folder is named by the UUID that ids::display maps to the id (as in t90).
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
on_disk() { [ -e "$P/states/$(dashed "$1")" ] || [ -e "$P/deltas/$(dashed "$1")" ]; }
wait_pin() { # the reader holds a pin: its rows are in the database
    for _ in $(seq 1 100); do [ "$(q "select count(*) from owners where kind='pin'")" -gt 0 ] && return 0; sleep 0.1; done
    return 1
}
hold() { # hold <dir> <out> <command...>: the reader runs in the background with the hook
    local dir=$1 out=$2; shift 2
    (cd "$dir" && env LAYR_TEST_PIN_HOLD_MS=4000 "$LAYR" "$@" > "$out" 2>&1; echo $? > "$out.rc") &
}

# Only the pins keep the states below: no undo record or deleted-line ring names them.
l config undo.ops 0 >/dev/null
l config undo.deleted 0 >/dev/null
l config retention.auto 1 >/dev/null

# 1. A merge source: its line is deleted while the merge holds the source.
l line new -q src >/dev/null
cd $M/src || exit 1
echo src change >> notes.txt; l save -qam "src change" >/dev/null
S=$(l rev-parse head)
cd $M/main || exit 1
echo main change > other.txt; l add other.txt >/dev/null; l save -qam "main change" >/dev/null
hold $M/main $T/merge.out merge "$S"
ok "the merge holds its source" wait_pin
l line delete -D src >/dev/null 2>&1
ok "a pinned merge source stays on disk after its last owner goes" on_disk "$S"
wait
eq "the merge finishes" 0 "$(cat $T/merge.out.rc)"
has "the merge has the source change" "src change" "$(l show head:notes.txt)"
has "the merge has the main change" "main change" "$(l show head:other.txt)"
# The merge commit names the source as its parent: the history keeps it.
ok "the merge keeps the source as its history" on_disk "$S"

# 2. The base of a prepared save: the head moves away while the save holds it.
l line new -q w >/dev/null
cd $M/w || exit 1
echo one >> notes.txt; l save -qam one >/dev/null
H=$(l rev-parse head)
echo two >> notes.txt
hold $M/w $T/save.out save -q
ok "the save holds its base" wait_pin
l reset -q head~1 >/dev/null
ok "a pinned save base stays on disk after its owner goes" on_disk "$H"
wait
eq "the save finishes" 0 "$(cat $T/save.out.rc)"
eq "the saved state has the working content" "two" "$(l show 'head@{0}:notes.txt' | tail -1)"
no "the save base goes after the save" on_disk "$H"


# A reader that comes later (no pin) gets a clear error for a freed state, never its content:
# a line whose head has no other owner is deleted, so its head goes at once.
l line new -q z >/dev/null
cd $M/z || exit 1
echo zz >> notes.txt; l save -qam zz >/dev/null
Z=$(l rev-parse head)
cd $M/main || exit 1
l line delete -D z >/dev/null 2>&1
has "a freed state is refused by a reader without a pin" "was freed" "$(l show "$Z:notes.txt" 2>&1 || true)"
no "the freed state is not read" l show "$Z:notes.txt"

# 3. An overlap compare: the other line's head moves away while status compares it.
cd $M/main || exit 1
l line delete -D w >/dev/null 2>&1
l line new -q a >/dev/null
l line new -q b >/dev/null
cd $M/a || exit 1
echo a change >> notes.txt; l save -qam "a change" >/dev/null
cd $M/b || exit 1
echo b change >> notes.txt; l save -qam "b change" >/dev/null
B=$(l rev-parse head)
hold $M/a $T/status.out status
ok "the status holds the compared heads" wait_pin
(cd $M/main && l line delete -D b >/dev/null 2>&1)
ok "a pinned compared head stays on disk after its owner goes" on_disk "$B"
wait
eq "the status finishes" 0 "$(cat $T/status.out.rc)"
has "the status reports the overlap with b" "notes.txt (b," "$(cat $T/status.out)"
no "the compared head goes after the status" on_disk "$B"
finish
