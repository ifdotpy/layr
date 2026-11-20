#!/bin/bash
# ci: macos overlayfs
# `layr add one-file` in a tree of 5000 files scans and compares only that path (and the ignore
# files above it), before the project lock, and it keeps deletes, type changes and moves right.
# A rewrite with equal bytes is no change. LAYR_TRACE reports each content comparison and, on
# APFS, the entries a path scan visited.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main
python3 - <<'PY'
import os
for d in range(50):
    os.makedirs(f"big/d{d}", exist_ok=True)
    for f in range(100):
        with open(f"big/d{d}/f{f}", "w") as h:
            h.write(f"{d} {f}")
PY
echo '*.log' > .gitignore
mkdir -p keep/sub other
echo a > keep/sub/a.txt
echo b > other/b.txt
l add . && l save -m "big tree" -q

# 1. One file: the scan visits few entries, and the comparison runs before the project lock.
# Equal sizes: only a content comparison tells the changed files from the others.
printf '7 X' > big/d7/f7
printf '8 X' > big/d8/f8
LAYR_TRACE=1 l add big/d7/f7 2> $T/trace.txt
compares=$(grep -c 'content compare' $T/trace.txt)
ok "add of one file compares at most 5 files ($compares)" test "$compares" -le 5
locked=$(awk '/store lock held/{on=0} on{print} /store lock taken/{on=1}' $T/trace.txt)
eq "add compares nothing under the project lock" "" "$(printf '%s\n' "$locked" | grep -E 'status|content compare')"
if [ "$(uname -s)" = Darwin ]; then
    visited=$(grep -o 'visited [0-9]*' $T/trace.txt | awk '{ if ($2 > m) m = $2 } END { print m + 0 }')
    has "the scan trace names its entries" "visited" "$(cat $T/trace.txt)"
    ok "add of one file visits at most 20 entries ($visited)" test "$visited" -ge 1 -a "$visited" -le 20
fi
st=$(l status --short 2>&1)
has "the named file is staged" "M  big/d7/f7" "$st"
has "the other file stays unstaged" " M big/d8/f8" "$st"
# A second add at once: the fresh stage cannot prove the read before the lock, which may run
# again under it, still for the named path only.
printf '9 X' > big/d9/f9
LAYR_TRACE=1 l add big/d9/f9 2> $T/trace.txt
compares=$(grep -c 'content compare' $T/trace.txt)
ok "a second add compares at most 10 files ($compares)" test "$compares" -le 10
has "the second file is staged" "M  big/d9/f9" "$(l status --short 2>&1)"
# An ignore file in a parent folder of the named path decides it.
echo '*.tmp' > big/d5/.gitignore && echo x > big/d5/a.tmp
no "a file ignored by a nested ignore file is not added" l add big/d5/a.tmp

# 2. Deleted folder, type change, move, ignored file.
rm -r keep/sub
rm other/b.txt && mkdir other/b.txt && echo x > other/b.txt/c
mv big/d1/f1 big/d1/moved
echo x > big/d1/a.log
l add keep other big/d1 2>&1
st=$(l status --short 2>&1)
has "deleted folder content is staged" "D  keep/sub/a.txt" "$st"
has "type change is staged" "other/b.txt/c" "$st"
has "move is staged" "big/d1/moved" "$st"
eq "ignored file is not added" 0 "$(echo "$st" | grep -c 'a.log')"
l add -f big/d1/a.log
has "an ignored file is added with -f" "A  big/d1/a.log" "$(l status --short 2>&1)"

# 3. A pattern.
echo p > big/d2/p1.txt; echo p > big/d2/p2.txt; echo q > big/d3/q.txt
l add 'big/d2/*.txt'
st=$(l status --short 2>&1)
has "pattern adds first" "A  big/d2/p1.txt" "$st"
has "pattern adds second" "A  big/d2/p2.txt" "$st"
eq "pattern leaves others" 0 "$(echo "$st" | grep -c '^A  big/d3/q.txt')"

# 4. Equal bytes rewritten: no change.
cp big/d4/f4 $T/f4.copy
cp $T/f4.copy big/d4/f4
touch big/d4/f4
eq "a rewrite with equal bytes is no change" 0 "$(l status --short 2>&1 | grep -c 'd4/f4')"

# 5. add reads the working folder before the project lock (only its line lease is held), so a
# command that writes the folder of the line holds that lease too: an overlay switch and park
# take it first. LAYR_TRACE prints a lock when it is taken: the order of the lines is the order.
first() { grep -n -m1 -F -- "$2" "$1" | cut -d: -f1; }
before() { # before <description> <trace> <first> <second>: both appear, the first one earlier
    local a b
    a=$(first "$2" "$3") b=$(first "$2" "$4")
    ok "$1" test -n "$a" -a -n "$b" -a "${a:-0}" -lt "${b:-0}"
}
l line new -q lease && cd ../lease
l overlay create other -q && printf 'C\n' > g
LAYR_TRACE=1 l overlay move -q other g 2> "$T/trace.txt"
before "overlay move takes the line lease before the project lock" "$T/trace.txt" "line lock taken: lease" "store lock taken: p"
l save -qm g
LAYR_TRACE=1 l overlay use --none -q 2> "$T/trace.txt"
before "overlay use takes the line lease before the project lock" "$T/trace.txt" "line lock taken: lease" "store lock taken: p"
echo changed >> notes.txt
LAYR_TRACE=1 l park -q 2> "$T/trace.txt"
before "park takes the line lease before its comparison" "$T/trace.txt" "line lock taken: lease" "status full comparison"
finish
