#!/bin/bash
# ci: macos overlayfs
# serial
# The project lock budget (issue #194): a command holds the project lock only for its commit. Each
# common command runs on a project with 2000 files, a cache folder and several lines; the test
# prints the longest project lock hold of each command and fails when one is over the limit.
# LAYR_LOCK_BUDGET_MS sets the limit. The design budget is 50 ms for a release build. CI runs the
# debug build (about 2.5 times slower here: 234 ms for `save -m` on an M-series Mac) on shared
# machines, so the default limit catches regressions of seconds and minutes, and the printed table
# shows the holds of each run.
. "$(dirname "$0")/lib.sh"

BUDGET=${LAYR_LOCK_BUDGET_MS:-1000}
sample $T/src
mkdir -p $T/src/many
for d in $(seq 1 20); do mkdir -p $T/src/many/d$d; for f in $(seq 1 100); do echo "$d $f" > $T/src/many/d$d/f$f; done; done
printf 'target regenerable\n' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target/release; for i in $(seq 1 200); do echo $i > target/release/f$i; done
l save --glasses -q
for n in a b c; do l line new -q $n main >/dev/null; done

# held <command...>: run it with the trace; print and return the longest hold of the project lock.
held() {
    local out ms
    out=$(LAYR_TRACE=1 "$LAYR" "$@" 2>&1 >/dev/null)
    ms=$(printf '%s\n' "$out" | awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }')
    printf '%6s ms  %s\n' "$ms" "$*" >&2
    echo "$ms"
}
within() { # within <command...>
    local ms
    ms=$(held "$@")
    ok "$* holds the project lock at most $BUDGET ms ($ms ms)" test "$ms" -le "$BUDGET"
}

cd $P/a || exit 1
for f in $(seq 1 50); do echo changed >> many/d1/f$f; done
within save
within add many/d1
within save -m "fifty files"
echo more >> notes.txt
within status
within restore notes.txt
within undo
echo edit >> many/d2/f1
within save -a -m "one more"
cd $P/main || exit 1
within line new d a
within merge a
within line delete -D d
within rev-parse head
within overlay list
echo bin2 > target/release/f1
within save --glasses
within gc --repair -q
finish
