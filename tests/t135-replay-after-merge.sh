#!/bin/bash
# ci: macos overlayfs
# `layr replay` and `layr pick x..y` after a merge of the upstream into the line: the merge base is
# then a second parent, and the chain of the line's own states stops at the first state the
# upstream has (it does not walk to the root, layr#251).
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init rp --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/rp/lines
cd "$P/main" || exit 1
l line new -q feat
cd "$P/feat" && echo f1 > f1.txt && l add f1.txt && l save -qm f1
cd "$P/main" && echo m1 > m1.txt && l add m1.txt && l save -qm m1
cd "$P/feat" && l merge main >/dev/null 2>&1
echo f2 > f2.txt && l add f2.txt && l save -qm f2
cd "$P/main" && echo m2 > m2.txt && l add m2.txt && l save -qm m2
m2=$(l rev-parse head)

cd "$P/feat" || exit 1
out=$(l replay main 2>&1)
rc=$?
[ $rc -eq 0 ] || printf '%s\n' "$out"
eq "replay after a merge of main succeeds" 0 "$rc"
eq "only the line's own states are replayed" "f2 f1 m2" "$(l log -3 --format=%s | tr '\n' ' ' | sed 's/ $//')"
eq "the replay starts at main" "$m2" "$(l rev-parse head~2)"
eq "the files of both lines are there" "f1 f2 m1 m2" "$(cat f1.txt f2.txt m1.txt m2.txt | tr '\n' ' ' | sed 's/ $//')"

# pick x..y takes the states of y that x does not have, also when x is on a second parent.
cd "$P/main" || exit 1
l line new -q other
cd "$P/other" && echo o1 > o1.txt && l add o1.txt && l save -qm o1
cd "$P/main" && echo m3 > m3.txt && l add m3.txt && l save -qm m3
cd "$P/other" && l merge main >/dev/null 2>&1
echo o2 > o2.txt && l add o2.txt && l save -qm o2
cd "$P/main" || exit 1
out=$(l pick main..other 2>&1)
rc=$?
[ $rc -eq 0 ] || printf '%s\n' "$out"
eq "pick of main..other succeeds" 0 "$rc"
eq "pick brings the files of o1 and o2" "o1 o2" "$(cat o1.txt o2.txt | tr '\n' ' ' | sed 's/ $//')"
finish
