#!/bin/bash
# ci: macos
# A named save (`save -m`) killed at each point of its commit (issue #194). The next command recovers
# the project: a save killed before its record leaves no state, keeps the staged files and removes the
# clone; a save killed after its record keeps the state and drops the stage. In every case no pending
# folder or journal is left. Test hook: LAYR_TEST_SAVE_KILL=<point> (SIGKILL there).
# The index clone is an APFS operation: other systems run no checks here.
[ "$(uname -s)" = Darwin ] || { echo "$(basename "$0"): 0 passed, 0 failed (APFS only)"; exit 0; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init crash --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/crash/lines/main
PROJ=$LAYR_ROOT/projects/crash
cd "$P" || exit 1

# Each point starts from a fresh head with one staged file.
start() {
    printf 'base\n' > base.txt && l add base.txt && l save -qm base
    printf 'staged %s\n' "$1" > "staged-$1.txt" && l add "staged-$1.txt"
}

kill_save() { # kill_save <point> <message>
    LAYR_TEST_SAVE_KILL=$1 l save -m "$2" > "$T/kill-$1.out" 2>&1
    eq "the save is killed at $1" "137" "$?"
}

# Killed before the record: no state, the staged file stays staged, the clone is removed.
for point in pending-cloned pending-renamed before-record; do
    start "$point"
    states=$(ls "$PROJ/states" | wc -l)
    kill_save "$point" "killed $point"
    l line list >/dev/null 2>&1
    eq "no state for a save killed at $point" "" "$(l log --format=%s | grep "killed $point")"
    eq "the staged file stays staged after $point" "A  staged-$point.txt" "$(l status --short | grep "staged-$point")"
    eq "no state folder is left after $point" "$states" "$(ls "$PROJ/states" | wc -l)"
    ok "no pending folder after $point" test -z "$(ls -A "$PROJ/meta/pending" 2>/dev/null)"
done

# Killed after the record: the state is kept, the stage is dropped, the pending folder is finished.
for point in after-record after-drop; do
    start "$point"
    kill_save "$point" "killed $point"
    l line list >/dev/null 2>&1
    has "the state of a save killed at $point is kept" "killed $point" "$(l log --format=%s)"
    eq "nothing is left staged after $point" "" "$(l status --short | grep "staged-$point")"
    ok "no pending folder after $point" test -z "$(ls -A "$PROJ/meta/pending" 2>/dev/null)"
done

# A save that completes leaves nothing behind.
start clean
ok "a save completes" l save -qm clean
ok "no pending folder after a save" test -z "$(ls -A "$PROJ/meta/pending" 2>/dev/null)"

finish
