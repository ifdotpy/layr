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
# A foreground pre-command save uses the same preparation. Three intervening automatic
# saves invalidate it; the fourth attempt uses the original locked save.
unset LAYR_SOCKET
export LAYR_DIRECT=1
W=$T/foreground-wait
mkdir -p "$W"
LAYR_TEST_AUTO_SAVE_WAIT="$W" LAYR_TRACE=1 l rm --cached notes.txt >"$T/fallback.log" 2>&1 &
fg=$!
for n in 1 2 3; do
    for _ in $(seq 1 600); do [ -e "$W/planned.$n" ] && break; sleep 0.1; done
    ok "foreground preparation $n reached" test -e "$W/planned.$n"
    printf 'change %s\n' "$n" > retry.txt
    l save --note "race $n" -q
    touch "$W/go.$n"
done
wait "$fg" && rc=0 || rc=$?
eq "foreground save falls back after three races" 0 "$rc"
ok "locked fallback is traced" grep -q 'save: locked fallback' "$T/fallback.log"
l add notes.txt

# A copied private overlay file must stay out of the automatic state; this also exercises
# comparison of the filtered, read-only snapshot on btrfs.
printf 'private content\n' > private.txt
l overlay create secret -q
l overlay move secret private.txt >/dev/null
l save -qm private
cp private.txt private-copy.txt
printf 'ordinary\n' > visible.txt
ok "pre-command save handles an overlay and copied private file" l rm --cached notes.txt
no "automatic state excludes copied private content" l show 'head@{0}:private-copy.txt'
ok "automatic state keeps ordinary content" l show 'head@{0}:visible.txt'

# Crash in the middle of the two cache-directory renames, then enter a pre-command save.
sample "$T/crash-src"
printf 'cache regenerable\n' > "$T/crash-src/.layrattributes"
# A regenerable folder is a cache only when it is ignored too.
printf 'cache/\n' >> "$T/crash-src/.gitignore"
l init crash --from "$T/crash-src" -q >/dev/null
cd "$LAYR_ROOT/projects/crash/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l overlay create cache.cache --folder cache --history last --quiet --merge union-theirs -q
mkdir cache
printf 'preserved\n' > cache/data
LAYR_TEST_CACHE_CRASH=between-renames l overlay repair cache.cache >"$T/crash.log" 2>&1 && crash_rc=0 || crash_rc=$?
eq "fixture exits between cache renames" 86 "$crash_rc"
ok "pre-command save recovers the interrupted cache replacement" l rm --cached notes.txt
eq "recovery preserves cache contents" preserved "$(cat cache/data)"
no "recovery removes its journal" sh -c 'ls .layr-cache-replace-*.json >/dev/null 2>&1'
finish
