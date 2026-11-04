#!/bin/bash
# ci: macos
# A named save (`save -m`) holds its line lock from the clone of the index to the record (issue
# #194). An `add` in the same line waits for the save and is not lost; an `add` in another line does
# not wait. Test hook: LAYR_TEST_SAVE_PAUSE_MS keeps the save open after its clone.
# The index clone is an APFS operation: other systems run no checks here.
[ "$(uname -s)" = Darwin ] || { echo "$(basename "$0") (APFS only): 0 passed, 0 failed"; exit 0; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init lk --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/lk/lines
PROJ=$LAYR_ROOT/projects/lk
cd "$P/main" || exit 1
printf 'base\n' > base.txt && l add base.txt && l save -qm base
printf 'slow\n' > slow.txt && l add slow.txt

# The save pauses for 3 s after its clone, with its line lock held.
LAYR_TEST_SAVE_PAUSE_MS=3000 l save -m "slow save" > "$T/slow.out" 2>&1 &
SAVE=$!
for _ in $(seq 1 100); do [ -n "$(ls -A "$PROJ/meta/pending" 2>/dev/null)" ] && break; sleep 0.1; done
ok "the save has cloned the index" test -n "$(ls -A "$PROJ/meta/pending" 2>/dev/null)"

now() { python3 -c 'import time; print(time.time())'; }
t0=$(now)
printf 'added\n' > added.txt && l add added.txt
waited=$(python3 -c "import sys; print(1 if float(sys.argv[1])-float(sys.argv[2]) > 1.5 else 0)" "$(now)" "$t0")
eq "an add in the same line waits for the save" "1" "$waited"
wait $SAVE
eq "the save is recorded" "" "$(grep -v '^$' "$T/slow.out" | grep -i fatal)"
has "the save made its state" "slow save" "$(l log --format=%s)"
has "the add made during the save is not lost" "A  added.txt" "$(l status --short)"
ok "no pending folder is left" test -z "$(ls -A "$PROJ/meta/pending" 2>/dev/null)"

# An add in another line does not wait for a save in this line.
l line new -q other main >/dev/null 2>&1
printf 'x\n' > x.txt && l add x.txt && l save -qm x > /dev/null 2>&1
printf 'pausing\n' > pausing.txt && l add pausing.txt
LAYR_TEST_SAVE_PAUSE_MS=3000 l save -m "pausing save" > "$T/pausing.out" 2>&1 &
SAVE=$!
for _ in $(seq 1 100); do [ -n "$(ls -A "$PROJ/meta/pending" 2>/dev/null)" ] && break; sleep 0.1; done
cd "$P/other" || exit 1
t0=$(now)
printf 'other\n' > other.txt && l add other.txt
quick=$(python3 -c "import sys; print(1 if float(sys.argv[1])-float(sys.argv[2]) < 1.5 else 0)" "$(now)" "$t0")
eq "an add in another line does not wait" "1" "$quick"
cd "$P/main" || exit 1
wait $SAVE

finish
