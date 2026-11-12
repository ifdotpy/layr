#!/bin/bash
# ci: macos overlayfs
# Line locks: a merge holds the lock of its line while it writes the working folder, and not the
# project lock. A save in another line finishes meanwhile; a second merge in the same line waits. A
# head that another command moved during a merge is refused with the clear message, and the folder
# is as it was. A crash after the folder write and before the record loses no work.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init locks --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/locks/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
for name in a b x y c; do l line new -q $name main; done
cd "$P/a" || exit 1
echo a > a.txt && l add a.txt && l save -qm a1
cd "$P/x" || exit 1
echo x > x.txt && l add x.txt && l save -qm x1
cd "$P/y" || exit 1
echo y > y.txt && l add y.txt && l save -qm y1
cd "$P/b" || exit 1
echo b > b.txt && l add b.txt && l save -qm b1

# 1. A merge in line a holds the line lock for 6 s. A save in line b does not wait for it. A second
# merge in line a waits for the first one.
stamp() { date +%s.%N; }
cd "$P/a" || exit 1
( LAYR_TEST_LINE_LOCK_MS=6000 "$LAYR" merge -q x > "$T/merge1.log" 2>&1; stamp > "$T/merge1.end" ) &
M1=$!
sleep 1
cd "$P/b" || exit 1
echo bb >> notes.txt
( "$LAYR" save -qa -m b2 > "$T/save-b.log" 2>&1; stamp > "$T/save-b.end" ) &
SB=$!
wait $SB
cd "$P/a" || exit 1
( "$LAYR" merge -q y > "$T/merge2.log" 2>&1; stamp > "$T/merge2.end" ) &
M2=$!
wait $M1 $M2
eq "the first merge printed no error" "" "$(grep -i error "$T/merge1.log" || true)"
ok "the save in line b finished" test -s "$T/save-b.end"
ok "the save in line b did not fail" test ! -s "$T/save-b.log"
ok "the second merge finished" test -s "$T/merge2.end"
ok "line b finished before the merge of line a" test "$(echo "$(cat "$T/save-b.end") < $(cat "$T/merge1.end")" | bc)" = 1
ok "the second merge in line a waited for the first" test "$(echo "$(cat "$T/merge2.end") > $(cat "$T/merge1.end")" | bc)" = 1
has "the line lock log names line a with a wait" "line=a" "$(cat "$LAYR_ROOT/projects/locks/meta/lock-slow.log" 2>/dev/null)"
eq "line a has both merged files" "1" "$(test -f "$P/a/x.txt" && test -f "$P/a/y.txt" && echo 1)"

# 2. A head that another command moved during the merge: refused with the message, and the folder
# is as it was before the command.
cd "$P/b" || exit 1
l status >/dev/null
before_head=$(l rev-parse head)
before_tree=$(find . -path ./.git -prune -o -type f -print | sort | xargs cat | shasum 2>/dev/null || find . -path ./.git -prune -o -type f -print | sort | xargs cat | md5sum)
trace=$(LAYR_TEST_MOVE_HEAD_AT_COMMIT=b "$LAYR" merge -q a 2>&1)
code=$?
eq "a refused move exits 1" "1" "$code"
has "a refused move names the line and both heads" "line b moved during the command (head" "$trace"
has "a refused move says to run it again" "run it again" "$trace"
eq "a refused move keeps the head" "$before_head" "$(l rev-parse head)"
after_tree=$(find . -path ./.git -prune -o -type f -print | sort | xargs cat | shasum 2>/dev/null || find . -path ./.git -prune -o -type f -print | sort | xargs cat | md5sum)
eq "a refused move keeps the working folder" "$before_tree" "$after_tree"
ok "a refused move leaves no merge file" test ! -f "$P/b/x.txt"

# 3. A crash after the folder write and before the record: the folder has the merged files, the head
# is the old one, and a later save records the work with no loss (the user's own edits stay).
cd "$P/c" || exit 1
echo c > c.txt && l add c.txt && l save -qm c1
echo mine > mine.txt
echo "local edit" >> notes.txt
crash_head=$(l rev-parse head)
LAYR_TEST_CRASH_BEFORE_COMMIT=c "$LAYR" merge -q a > "$T/crash.log" 2>&1
eq "the crash exits with the hook code" "86" "$?"
eq "the crash keeps the head" "$crash_head" "$(l rev-parse head)"
eq "the merged file is in the folder after the crash" "a" "$(cat a.txt)"
eq "the user's untracked file is in the folder" "mine" "$(cat mine.txt)"
eq "the user's edit is in the folder" "local edit" "$(tail -1 notes.txt)"
has "status shows the work as changes" "a.txt" "$(l status)"
ok "a save after the crash records the work" sh -c "'$LAYR' add a.txt mine.txt && '$LAYR' save -qam recovered"
eq "the saved state has the merged file" "a" "$(l show head:a.txt)"
eq "the saved state has the user's edit" "local edit" "$(l show head:notes.txt | tail -1)"
eq "the saved state has the added file" "mine" "$(l show head:mine.txt)"

# 4. The cache hook takes the line lock of its line. `save --overlays` with a prune adapter set, and with
# prune=none, finishes: the hook does not wait for a second file lock of the same line.
# bounded <seconds> <command...>: the exit code of the command, or 124 when it runs longer.
bounded() {
    local secs=$1; shift
    "$@" &
    local pid=$! i=0
    while kill -0 "$pid" 2>/dev/null; do
        i=$((i + 1))
        if [ "$i" -gt $((secs * 5)) ]; then
            kill "$pid" 2>/dev/null
            wait "$pid" 2>/dev/null
            return 124
        fi
        sleep 0.2
    done
    wait "$pid"
}
l line new -q k main
cd "$P/k" || exit 1
printf 'cachedir/\n' >> .gitignore
printf 'cachedir regenerable\n' > .layrattributes
l add -A && l save -qm attributes
mkdir -p cachedir && echo stale > cachedir/a.bin
bounded 60 "$LAYR" save --overlays -q > "$T/caches-1.log" 2>&1
eq "save --overlays makes the cache overlay" "0" "$?"
has "the overlay of the folder exists" "folder=cachedir" "$(l overlay list)"
l overlay set "$(overlay cachedir)" --prune nosuch >/dev/null
echo fresh > cachedir/a.bin
bounded 60 "$LAYR" save --overlays -q > "$T/caches-2.log" 2>&1
eq "save --overlays with an unknown prune adapter finishes" "0" "$?"
l overlay set "$(overlay cachedir)" --prune none >/dev/null
echo third > cachedir/a.bin
bounded 60 "$LAYR" save --overlays -q > "$T/caches-3.log" 2>&1
eq "save --overlays with prune none finishes" "0" "$?"
eq "prune none keeps the folder content" "third" "$(cat cachedir/a.bin)"
finish
