#!/bin/bash
# `layr add one-file` in a tree of 5000 files scans and compares only that path (and the ignore
# files above it), and it keeps deletes, type changes and moves right. A rewrite with equal
# bytes is no change. LAYR_TRACE reports the entries a path scan visited.
. "$(dirname "$0")/lib.sh"

[ "$(uname -s)" = Darwin ] || { echo "$(basename "$0"): 0 passed, 0 failed (APFS only)"; exit 0; }
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

# 1. One file: the scan visits few entries.
echo changed >> big/d7/f7
echo more >> big/d8/f8
LAYR_TRACE=1 l add big/d7/f7 2> $T/trace.txt
visited=$(grep -o 'visited [0-9]*' $T/trace.txt | awk '{ if ($2 > m) m = $2 } END { print m + 0 }')
has "the scan trace names its entries" "visited" "$(cat $T/trace.txt)"
ok "add of one file visits at most 20 entries ($visited)" test "$visited" -ge 1 -a "$visited" -le 20
st=$(l status --short 2>&1)
has "the named file is staged" "M  big/d7/f7" "$st"
has "the other file stays unstaged" " M big/d8/f8" "$st"

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
finish
