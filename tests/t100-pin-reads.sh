#!/bin/bash
# ci: macos overlayfs
# Pins (docs/ownership.md, "Pins"): a command that reads saved states keeps them on disk until it
# ends, though another command frees their last owner meanwhile. Test hook:
# LAYR_TEST_SAVE_PIN_BARRIER holds the reader with its pins taken until the test releases it (test
# only). A merge source, a direct reader and an overlap compare survive concurrent ownership removal.
# A save also holds its line lock: its reset must wait, and its automatic state keeps the old parent
# until replaced.
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
on_disk() { state_available "$P" "$(dashed "$1")"; }
hold() { # hold <dir> <out> <command...>: the reader runs in the background and stops at its first pins
    local dir=$1; READER=$2; shift 2
    mkdir "$READER.gate"
    (cd "$dir" && env LAYR_TEST_SAVE_PIN_BARRIER="$READER.gate" "$LAYR" "$@" > "$READER" 2>&1; echo $? > "$READER.rc") &
}
wait_pin() { # wait_pin <state>: the held reader is stopped and its pin of the state is in the database
    for _ in $(seq 1 600); do # a hang guard only: a reader that ends early stops the wait at once
        if [ -f "$READER.gate/ready" ] || [ -f "$READER.rc" ]; then break; fi
        sleep 0.1
    done
    [ -f "$READER.gate/ready" ] && [ "$(q "select count(*) from owners where kind='pin' and state='$(dashed "$1")'")" -gt 0 ]
}
release() { touch "$READER.gate/release"; wait; } # the held reader goes on to its end

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
ok "the merge holds its source" wait_pin "$S"
ok "the source line goes while the merge holds it" l line delete -D src
ok "a pinned merge source stays on disk after its last owner goes" on_disk "$S"
release
eq "the merge finishes" 0 "$(cat $T/merge.out.rc)"
has "the merge has the source change" "src change" "$(l show head:notes.txt)"
has "the merge has the main change" "main change" "$(l show head:other.txt)"
# The merge commit names the source as its parent: the history keeps it.
ok "the merge keeps the source as its history" on_disk "$S"

# 2. The base of a prepared save: reset waits for the save's line lock. The save
# retains its parent through the newest automatic state. Replacing that state after
# reset drops the last owner; its native storage must then be reclaimed.
l line new -q w >/dev/null
cd $M/w || exit 1
echo one >> notes.txt; l save -qam one >/dev/null
H=$(l rev-parse head)
echo two >> notes.txt
python3 - "$T" "$LAYR" "$M/w" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import time

root, binary, work = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
gate = root / 'save-gate'
gate.mkdir()
reset = None
with (root / 'save.out').open('w') as log:
    saver = subprocess.Popen([binary, 'save', '-q'], cwd=work,
        env=dict(os.environ, LAYR_TEST_SAVE_PUBLISH_BARRIER=str(gate)), stdout=log, stderr=log)
try:
    deadline = time.monotonic() + 20
    while not (gate / 'ready').exists():
        assert saver.poll() is None, (root / 'save.out').read_text()
        assert time.monotonic() < deadline
        time.sleep(.01)
    with (root / 'reset.out').open('w') as log:
        reset = subprocess.Popen([binary, 'reset', '-q', 'head~1'], cwd=work, stdout=log, stderr=log)
    try:
        reset.wait(timeout=.5)
        raise AssertionError('reset completed while the save held its line lock')
    except subprocess.TimeoutExpired:
        pass
    (gate / 'release').touch()
    code = saver.wait(timeout=20)
    (root / 'save.out.rc').write_text(str(code))
    assert code == 0, (root / 'save.out').read_text()
    assert reset.wait(timeout=20) == 0, (root / 'reset.out').read_text()
finally:
    for process in (saver, reset):
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
PY
ok "reset waits for the save's line lock" test "$?" = 0
eq "the save finishes" 0 "$(cat $T/save.out.rc)"
eq "the saved state has the working content" "two" "$(l show 'head@{0}:notes.txt' | tail -1)"
ok "the serialized save keeps its parent through its automatic state" on_disk "$H"
l save --force -q >/dev/null
no "the save base goes after its last automatic owner goes" on_disk "$H"

# A direct reader has no line lock: deletion removes the state's last non-pin owner
# while show is reading. Only the reader's pin can keep this leaf state alive.
l line new -q pin-only main >/dev/null
cd $M/pin-only || exit 1
echo pin-only >> notes.txt; l save -qam pin-only >/dev/null
R=$(l rev-parse head)
cd $M/main || exit 1
hold $M/main $T/read.out show "$R:notes.txt"
ok "the direct reader holds its state" wait_pin "$R"
ok "the line goes while the reader holds its state" l line delete -D --discard pin-only
eq "only the reader's pin still owns the state" 0 "$(q "select count(*) from owners where state='$(dashed "$R")' and kind!='pin'")"
ok "the pin keeps the state after concurrent deletion" on_disk "$R"
release
eq "the direct reader finishes" 0 "$(cat $T/read.out.rc)"
has "the direct reader returns its pinned content" "pin-only" "$(cat $T/read.out)"
no "the state goes when its only pin is released" on_disk "$R"


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
l line delete -D --discard w >/dev/null 2>&1
l line new -q a >/dev/null
l line new -q b >/dev/null
cd $M/a || exit 1
echo a change >> notes.txt; l save -qam "a change" >/dev/null
cd $M/b || exit 1
echo b change >> notes.txt; l save -qam "b change" >/dev/null
B=$(l rev-parse head)
cd $M/main || exit 1
hold $M/a $T/status.out status
ok "the status holds the compared heads" wait_pin "$B"
ok "the compared line goes while the status holds its head" l line delete -D b
ok "a pinned compared head stays on disk after its owner goes" on_disk "$B"
release
eq "the status finishes" 0 "$(cat $T/status.out.rc)"
has "the status reports the overlap with b" "notes.txt (b," "$(cat $T/status.out)"
no "the compared head goes after the status" on_disk "$B"
finish
