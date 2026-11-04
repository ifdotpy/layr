#!/bin/bash
# ci: macos overlayfs
# save --caches makes the snapshot of the working folder and its compare with the head before the
# project lock (issue #194). Under the lock it uses them when the folder did not change since; a
# folder that changed after the prepare step (LAYR_TEST_SAVE_PAUSE_MS) is saved as it is then.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf 'target regenerable\n' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main >/dev/null 2>&1 || true
head=$(l rev-parse head)
saved() { l rev-parse "$(printf '%s\n' "$1" | awk '$1 == "main" { print $2 }' | tail -1)"; }

# 1. A protected line with no changes since its head gives its caches to the head.
mkdir -p target/release; echo bin1 > target/release/x
out=$(LAYR_TRACE=1 l save --caches 2>&1)
eq "the caches go to the head" "$head" "$(saved "$out")"
[ "$(uname -s)" = Darwin ] && has "the state is prepared before the lock" "prepare caches state" "$(printf '%s\n' "$out" | sed '/store lock taken: p$/q')"
l line new -q warm main >/dev/null
eq "a new line from main has the cache" bin1 "$(cat $P/warm/target/release/x)"

# 2. New times on tracked files are no change: the caches still go to the head.
echo bin2 > target/release/x
touch notes.txt src/main.rs
eq "new times keep the head" "$head" "$(saved "$(l save --caches 2>&1)")"

# 3. A tracked file that changes after the prepare step: the state under the lock has the change.
echo bin3 > target/release/x
LAYR_TEST_SAVE_PAUSE_MS=3000 l save --caches >$T/late.out 2>&1 &
pid=$!
sleep 1
echo late >> notes.txt
wait $pid
eq "the save succeeds" 0 "$?"
late=$(saved "$(cat $T/late.out)")
ok "the caches go to a new state" test "$late" != "$head"
has "the new state has the change made after the prepare step" late "$(l show "$late:notes.txt" 2>&1)"
eq "no prepared state stays in the work folder" "" "$(ls -A "$LAYR_ROOT/projects/p/meta/work" 2>/dev/null | grep -v '^prepared-' )"
ok "gc finds no leak" l gc -q
finish
