#!/bin/bash
# ci: macos
# A named save (`save -m`) killed at each point of its commit (issue #194). The next command recovers
# the line: a killed save before its record leaves no state and keeps the staged files; a save killed
# after its record keeps the state. Test hook: LAYR_TEST_SAVE_KILL=<point> (SIGKILL there).
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init crash --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/crash/lines/main
META=$LAYR_ROOT/projects/crash/meta
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

# Killed before the record: no state, the staged file stays staged, the journal is gone after recovery.
start before
kill_save before-record "killed before record"
l line list >/dev/null 2>&1
eq "no state for the killed save" "" "$(l log --format=%s | grep 'killed before record')"
has "the staged file stays staged" "A  staged-before.txt" "$(l status --short)"
ok "the journal is cleared" test ! -e "$META/apfs-index-commit.json"

# Killed after the record: the state is kept, the staged file is committed.
start after
kill_save after-record "killed after record"
l line list >/dev/null 2>&1
has "the state of the killed save is kept" "killed after record" "$(l log --format=%s)"
eq "nothing is left staged" "" "$(l status --short | grep 'staged-after')"
ok "the journal is cleared after the record" test ! -e "$META/apfs-index-commit.json"

if [ "$(uname -s)" = Darwin ]; then
    # Killed after the index was promoted to a state, before the record: the index is restored.
    start promoted
    kill_save promoted "killed promoted"
    l line list >/dev/null 2>&1
    eq "no state for the promoted save" "" "$(l log --format=%s | grep 'killed promoted')"
    has "the promoted index is restored to the stage" "A  staged-promoted.txt" "$(l status --short)"
    ok "the journal is cleared after the promoted save" test ! -e "$META/apfs-index-commit.json"
fi

# A save that completes leaves nothing behind.
start clean
ok "a save completes" l save -qm clean
ok "no journal after a save" test ! -e "$META/apfs-index-commit.json"

finish
