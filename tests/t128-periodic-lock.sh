#!/bin/bash
# ci: overlayfs
# The daemon prepares a periodic save without holding the project lock. Foreground
# operations can finish, and a changed line makes it retry its plan.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
printf 'target regenerable\n' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target
printf 'cache\n' > target/x
l save --caches -q
l line new -q other main >/dev/null
l config snapshot.interval 0
printf 'periodic\n' > "$P/main/periodic.txt"
mkdir -p "$T/wait"
export LAYR_SOCKET=$T/daemon.sock
LAYR_TRACE=1 LAYR_TEST_PERIODIC_WAIT="$T/wait" "$LAYR" daemon --socket "$LAYR_SOCKET" --tick 1 >"$T/daemon.log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null; cleanup' EXIT
for _ in $(seq 1 600); do [ -e "$T/wait/planned.main.1" ] && break; sleep 0.1; done
ok 'periodic save reaches its prepared snapshot' test -e "$T/wait/planned.main.1"
(
    cd "$P/other" || exit 1
    printf 'new cache\n' > target/x
    l save --caches -q || exit 1
    l rev-parse head >/dev/null || exit 1
    l op log >/dev/null || exit 1
    printf 'foreground\n' > foreground.txt
    l add foreground.txt || exit 1
    l save -qm foreground || exit 1
    cd "$P/main" || exit 1
    printf 'intervening\n' > intervening.txt
    l save --note intervening -q || exit 1
    printf 'after foreground save\n' > after.txt
    touch "$T/foreground.done"
) >"$T/foreground.log" 2>&1 &
fg=$!
for _ in $(seq 1 600); do [ -e "$T/foreground.done" ] && break; sleep 0.1; done
ok 'foreground reads and save finish while periodic preparation waits' test -e "$T/foreground.done"
kill "$fg" 2>/dev/null || true
wait "$fg" 2>/dev/null || true
touch "$T/wait/go.main.1" "$T/wait/go.main.2" "$T/wait/go.main.3" "$T/wait/go.other.1" "$T/wait/go.other.2" "$T/wait/go.other.3"
for _ in $(seq 1 600); do [ -e "$T/wait/committed.main.2" ] && break; sleep 0.1; done
ok 'the periodic pass commits after retrying' test -e "$T/wait/committed.main.2"
ok 'a changed line retries preparation' test -e "$T/wait/planned.main.2"
cd "$P/main" || exit 1
has 'the prepared periodic state is recorded' periodic "$(l log --auto --format=%s -n 3)"
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
ok 'the daemon reports its project lock holds' grep -q 'store lock held: p$' "$T/daemon.log"
held=$(awk '/store lock held: p$/ { if ($2 > m) m = $2 } END { print int(m) }' "$T/daemon.log")
ok "project lock hold remains below 2 seconds ($held ms)" test "$held" -lt 2000
finish
