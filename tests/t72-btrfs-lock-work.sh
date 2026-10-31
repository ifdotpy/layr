#!/bin/bash
# Work under the project lock on btrfs (LAYR_TRACE spans). `add` and `save -a` do not diff the
# head against the index; a save updates the git index for the changed paths only and does not
# refresh every tracked file; a line delete destroys the known nested subvolumes (.git and the
# saved caches) without a search of the whole working folder. Each check names a span that the
# old code never printed, or one that it printed.
. "$(dirname "$0")/lib.sh"

if [ "$(uname -s)" != Linux ] || [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    echo "t72-btrfs-lock-work.sh: 0 passed, 0 failed (btrfs only)"
    exit 0
fi

count() { # count <needle> <file>: the lines of the trace file with the needle
    grep -c -- "$1" "$2" || true
}

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
l line new rel main >/dev/null
cd $P/rel

# 1. add with an existing stage reads index to work only.
echo a > a.txt
l add a.txt
echo b > b.txt
LAYR_TRACE=1 l add b.txt 2> $T/add.trace
ok "add succeeds" test -s $T/add.trace
ok "add uses the status for add" test "$(count 'status for add' $T/add.trace)" -ge 1
eq "add does not diff the head against the index" 0 "$(count 'head to index diff' $T/add.trace)"
has "add staged both files" "b.txt" "$(l status -s)"
has "add kept the first staged file" "a.txt" "$(l status -s)"
echo c >> notes.txt
LAYR_TRACE=1 l save -a -m "with stage" -q 2> $T/save-a.trace
ok "save -a uses the status for add" test "$(count 'status for add' $T/save-a.trace)" -ge 1
eq "the save has the staged and the changed files" "a.txt b.txt notes.txt" "$(l show --name-only --format= head | sort | tr '\n' ' ' | sed 's/ $//')"

# 2. A save updates the git index for the changed paths only.
echo d > d.txt
l add d.txt
l save -q -m warm
echo more >> notes.txt
LAYR_TRACE=1 l save -a -m edit -q 2> $T/save.trace
eq "a save refreshes no tracked file of the git index" "" "$(grep 'git index refresh' $T/save.trace)"
has "a save updates the changed paths of the git index" "git index update of 1 changed paths" "$(cat $T/save.trace)"
eq "git status is clean after the save" "" "$(git status --short)"
l line new fresh rel >/dev/null
eq "git status is clean in a fresh line" "" "$(cd $P/fresh && git status --short)"

# 3. A line delete finds the nested subvolumes without a search.
cd $P/rel
rm -rf target; mkdir -p target/debug; echo bin > target/debug/x
l save --caches -q
cd $P/main
LAYR_TRACE=1 l line delete -D rel 2> $T/delete.trace >/dev/null
ok "the line folder is gone" test ! -e $P/rel
has "the known nested subvolumes are destroyed first" "delete: known nested subvolumes" "$(cat $T/delete.trace)"
eq "no search of the working folder for nested subvolumes" "" "$(grep 'full search for nested' $T/delete.trace)"
finish
