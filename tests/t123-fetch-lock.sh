#!/bin/bash
# ci: macos overlayfs
# Lock only for the commit (issue 194). A git fetch plans and transfers with no project lock: a slow
# fetch does not block a save in another line. Two fetches in one project both succeed and leave the
# same refs. A remote ref that moves while a fetch plans its import is planned again, and the
# fetch ends at the newest tip.
. "$(dirname "$0")/lib.sh"

stamp() { date +%s.%N; }
R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; for i in $(seq 1 20); do echo "base $i" > src/f$i.txt; done
git add -A; git commit -qm "upstream 0"; git push -q origin main

l git clone $R gp -q >/dev/null
P=$LAYR_ROOT/projects/gp/lines
cd $P/main || exit 1
for name in a b; do l line new -q $name origin/main; done
upstream() { # upstream <from> <to>: commits "upstream N" on the remote main
    cd $U || exit 1
    for i in $(seq "$1" "$2"); do echo "change $i" >> src/f$((i % 20 + 1)).txt; git commit -qam "upstream $i"; done
    git push -q origin main
}

# 1. A slow fetch (a test hook sleeps 6 s after its transfer) does not block a save in line a.
upstream 1 5
cd $P/main || exit 1
( LAYR_TEST_FETCH_SLEEP_MS=6000 "$LAYR" git fetch origin main > "$T/slow.log" 2>&1; stamp > "$T/slow.end" ) &
SF=$!
sleep 1
cd $P/a || exit 1
echo x > x.txt && l add x.txt >/dev/null
l save -qm a1 > "$T/save.log" 2>&1
stamp > "$T/save.end"
wait $SF
eq "the save in line a did not fail" "" "$(cat "$T/save.log")"
ok "the save in line a ended before the slow fetch" test "$(echo "$(cat "$T/save.end") < $(cat "$T/slow.end")" | bc)" = 1
eq "the slow fetch printed no error" "" "$(grep -i 'error\|fatal' "$T/slow.log" || true)"
eq "the slow fetch imported the five commits" "upstream 5" "$(cd $P/main && l log -1 --format=%s origin/main)"

# 2. Two fetches in one project run at once: one of them sleeps in its transfer. Both succeed, and both
# see the same refs. The history of origin/main has every commit once.
upstream 6 8
cd $P/a || exit 1
( LAYR_TEST_FETCH_SLEEP_MS=3000 "$LAYR" git fetch origin main > "$T/c1.log" 2>&1; echo $? > "$T/c1.code" ) &
C1=$!
cd $P/b || exit 1
( "$LAYR" git fetch origin main > "$T/c2.log" 2>&1; echo $? > "$T/c2.code" ) &
C2=$!
wait $C1 $C2
eq "the first concurrent fetch exits 0" "0" "$(cat "$T/c1.code")"
eq "the second concurrent fetch exits 0" "0" "$(cat "$T/c2.code")"
eq "line a sees the newest tip" "upstream 8" "$(cd $P/a && l log -1 --format=%s origin/main)"
eq "line b sees the newest tip" "upstream 8" "$(cd $P/b && l log -1 --format=%s origin/main)"
eq "origin/main has every commit once" "$(git -C $R rev-list --count main)" "$(cd $P/a && l log --format=%s origin/main | wc -l | tr -d ' ')"
eq "the store is consistent after both fetches" "0 problem(s)" "$(cd $P/a && l fsck | tail -1)"

# 3. The remote ref moves while fetch a plans its import (the hook sleeps 4 s after planning). Fetch b
# stores a newer tip meanwhile. Fetch a plans again from the stored refs and ends at the newest tip.
upstream 9 9
cd $P/a || exit 1
( LAYR_TEST_PLAN_SLEEP_MS=4000 "$LAYR" git fetch origin main > "$T/m1.log" 2>&1; echo $? > "$T/m1.code" ) &
M1=$!
sleep 2
upstream 10 10
cd $P/b || exit 1
"$LAYR" git fetch origin main > "$T/m2.log" 2>&1
wait $M1
eq "the fetch that planned while the ref moved exits 0" "0" "$(cat "$T/m1.code")"
eq "the moved fetch prints no error" "" "$(grep -i 'error\|moved' "$T/m1.log" || true)"
eq "fetch a ends at the newest tip" "upstream 10" "$(cd $P/a && l log -1 --format=%s origin/main)"
eq "the newest tip is the remote tip" "$(git -C $R rev-parse main)" "$(cd $P/a && l rev-parse --git origin/main)"
eq "origin/main still has every commit once" "$(git -C $R rev-list --count main)" "$(cd $P/a && l log --format=%s origin/main | wc -l | tr -d ' ')"
finish
