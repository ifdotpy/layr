#!/bin/bash
# ci: macos
# `queue add <line> -m "<conventional title>"`: the title must follow the Conventional Commit rule, and the queue
# merges the entry as one state with that title (a squash): the target gets one state for the whole line.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
before=$(l rev-parse head)
l line new -q topic
cd "$P/topic" || exit 1
echo one > one.txt; l add one.txt; l save -qm one
echo two > two.txt; l add two.txt; l save -qm two
no "a title that is not conventional is refused" l queue add topic -m "two saves"
has "the refusal names the rule" "Conventional Commit" "$(l queue add topic -m "two saves" 2>&1)"
ok "a conventional title is taken" l queue add topic -m "feat(topic): two files" --json
cd "$P/main" || exit 1
ok "the queue merges the entry" l queue run --into main
eq "main has both files" "one two" "$(cat one.txt two.txt 2>/dev/null | tr '\n' ' ' | sed 's/ $//')"
has "main's head carries the title" "feat(topic): two files" "$(l log -n1)"
eq "one state on main for the whole line" "$before" "$(l rev-parse 'head~1')"
finish
