#!/bin/bash
# ci: macos overlayfs
# A head change after the follow build must not be recorded as a warm cache for the new head.
. "$(dirname "$0")/lib.sh"
skip_in_check_sandbox "its cache worker runs cargo with the OS home"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
mkdir -p "$T/home" "$T/src/src" "$T/barrier"
if [ "$(uname -s)" = Darwin ]; then export HOME="$T/home"; else export LAYR_SOCKET="$T/service.sock"; fi
export CARGO_NET_OFFLINE=true
printf '[package]\nname = "follow-cache-race"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [["build", "--offline"]]\n' > "$T/src/Cargo.toml"
printf 'fn main() { println!("one"); }\n' > "$T/src/src/main.rs"
printf 'target regenerable\n' > "$T/src/.layrattributes"
printf '/target\n' > "$T/src/.gitignore"
(cd "$T/src" && cargo generate-lockfile --offline) || exit 1
l init race --from "$T/src" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/race/lines/main"
git init -q --bare "$T/origin.git" || exit 1
l git remote add origin "$T/origin.git" || exit 1
l git push -q origin main || exit 1
cargo build --offline -q || exit 1
l save --caches -q || exit 1
l follow origin/main > "$T/first.log" 2>&1 || { cat "$T/first.log"; exit 1; }
l line new other main -q || exit 1
printf 'fn main() { println!("two"); }\n' > src/main.rs
l save -a -q -m two || exit 1
LAYR_TEST_FOLLOW_CACHE_BARRIER="$T/barrier" "$LAYR" follow --run > "$T/follow.log" 2>&1 &
FOLLOW=$!
for _ in $(seq 1 600); do
    [ -f "$T/barrier/ready" ] && break
    kill -0 "$FOLLOW" 2>/dev/null || break
    sleep 0.1
done
if [ ! -f "$T/barrier/ready" ]; then
    cat "$T/follow.log"; wait "$FOLLOW"; echo 'FATAL: follow did not reach the barrier'; exit 1
fi
printf 'fn main() { println!("three"); }\n' > src/main.rs
(l save -a -q -m three > "$T/save.log" 2>&1; printf '%s' "$?" > "$T/save.result") &
SAVE=$!
sleep 1
no "the head change waits for the cache save" test -f "$T/save.result"
(cd "$LAYR_ROOT/projects/race/lines/other" && printf 'fn main() { println!("other"); }\n' > src/main.rs && l save -a -q -m other) > "$T/other.log" 2>&1 &
OTHER=$!
for _ in $(seq 1 50); do kill -0 "$OTHER" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$OTHER" 2>/dev/null; then
    touch "$T/barrier/release"; wait "$FOLLOW"; wait "$SAVE"; wait "$OTHER"
    echo 'FATAL: another line did not progress'; exit 1
fi
wait "$OTHER"; rc=$?
eq "another line can save while the cache build is paused" 0 "$rc"
touch "$T/barrier/release"
wait "$FOLLOW" || { cat "$T/follow.log"; wait "$SAVE"; exit 1; }
wait "$SAVE"
eq "the waiting head change succeeds" 0 "$(cat "$T/save.result")"
# If the first pass could not build for the new head, the next pass must build it.
out=$(l follow --run 2>&1)
has "the next follow pass rebuilds after the head changes" 'caches saved with the head' "$out"
eq "the cache binary reflects the new head" three "$(target/debug/follow-cache-race)"
# A prepared cache save and a follow build on one line must not publish out of order.
mkdir -p "$T/cache-save-barrier"
LAYR_TEST_CACHE_SAVE_BARRIER="$T/cache-save-barrier" "$LAYR" save --caches -q > "$T/cache-save.log" 2>&1 &
CACHE_SAVE=$!
for _ in $(seq 1 300); do
    [ -f "$T/cache-save-barrier/ready" ] && break
    kill -0 "$CACHE_SAVE" 2>/dev/null || break
    sleep 0.1
done
if [ ! -f "$T/cache-save-barrier/ready" ]; then
    cat "$T/cache-save.log"; wait "$CACHE_SAVE"; echo 'FATAL: cache save did not reach the barrier'; exit 1
fi
printf 'fn main() { println!("four"); }\n' > src/main.rs
(l save -a -q -m four > "$T/four.log" 2>&1; printf '%s' "$?" > "$T/four.result") &
SAVE_FOUR=$!
sleep 1
no "a head change waits for a prepared cache save" test -f "$T/four.result"
touch "$T/cache-save-barrier/release"
wait "$CACHE_SAVE"; cache_rc=$?
eq "the earlier cache save completes" 0 "$cache_rc"
wait "$SAVE_FOUR"
eq "the next head save completes" 0 "$(cat "$T/four.result")"
out=$(l follow --run 2>&1)
has "follow rebuilds after the cache save" 'caches saved with the head' "$out"
eq "the follow build contains the new value" four "$(target/debug/follow-cache-race)"
l line new verify main -q || exit 1
eq "a new line receives the latest cache" four "$("$LAYR_ROOT/projects/race/lines/verify/target/debug/follow-cache-race" 2>/dev/null)"

# Follow waits for a cache save that began before it and then publishes the rebuilt cache.
printf 'fn main() { println!("five"); }\n' > src/main.rs
l save -a -q -m five || exit 1
mkdir -p "$T/cache-save-before-follow"
LAYR_TEST_CACHE_SAVE_BARRIER="$T/cache-save-before-follow" "$LAYR" save --caches -q > "$T/old-cache.log" 2>&1 &
OLD_CACHE=$!
for _ in $(seq 1 300); do
    [ -f "$T/cache-save-before-follow/ready" ] && break
    kill -0 "$OLD_CACHE" 2>/dev/null || break
    sleep 0.1
done
if [ ! -f "$T/cache-save-before-follow/ready" ]; then
    cat "$T/old-cache.log"; wait "$OLD_CACHE"; echo 'FATAL: old cache save did not reach the barrier'; exit 1
fi
("$LAYR" follow --run > "$T/later-follow.log" 2>&1; printf '%s' "$?" > "$T/later-follow.result") &
LATER_FOLLOW=$!
sleep 1
no "follow waits for the earlier cache save" test -f "$T/later-follow.result"
touch "$T/cache-save-before-follow/release"
wait "$OLD_CACHE"; old_rc=$?
eq "the earlier cache save succeeds" 0 "$old_rc"
wait "$LATER_FOLLOW"
eq "the waiting follow pass succeeds" 0 "$(cat "$T/later-follow.result")"
has "the waiting follow pass builds" 'caches saved with the head' "$(cat "$T/later-follow.log")"
l line new final main -q || exit 1
eq "the final line receives the rebuilt cache" five "$("$LAYR_ROOT/projects/race/lines/final/target/debug/follow-cache-race" 2>/dev/null)"

# A crash after an unsuccessful build must leave the cache marked for another attempt.
printf 'fn main() { missing_function(); }\n' > src/main.rs
l save -a -q -m broken || exit 1
mkdir -p "$T/mark-barrier"
LAYR_TEST_FOLLOW_MARK_BARRIER="$T/mark-barrier" "$LAYR" follow --run > "$T/interrupted-follow.log" 2>&1 &
INTERRUPTED=$!
for _ in $(seq 1 300); do
    [ -f "$T/mark-barrier/ready" ] && break
    kill -0 "$INTERRUPTED" 2>/dev/null || break
    sleep 0.1
done
if [ ! -f "$T/mark-barrier/ready" ]; then
    cat "$T/interrupted-follow.log"; wait "$INTERRUPTED"; echo 'FATAL: follow did not reach the mark barrier'; exit 1
fi
kill -KILL "$INTERRUPTED"
wait "$INTERRUPTED" 2>/dev/null
has "the interrupted follow build failed" 'not built' "$(cat "$T/interrupted-follow.log")"
out=$(l follow --run 2>&1)
has "follow retries the interrupted unsuccessful build" 'not built' "$out"
printf 'fn main() { println!("six"); }\n' > src/main.rs
l save -a -q -m fixed || exit 1
out=$(l follow --run 2>&1)
has "follow builds after the source is fixed" 'caches saved with the head' "$out"
eq "the rebuilt executable uses the fixed source" six "$(target/debug/follow-cache-race)"
out=$(l follow --run 2>&1)
has "the next pass reports the cache is up to date" 'origin/main: up to date' "$out"
no "the next pass does not rebuild" grep -q 'caches saved with the head' <<< "$out"

# A killed cache save releases the line lock and leaves a usable project.
live_work="$LAYR_ROOT/projects/race/meta/work/prepared-11111111-2222-3333-4444-555555555555.$$"
unknown_work="$LAYR_ROOT/projects/race/meta/work/prepared-11111111-2222-3333-4444-555555555556"
mkdir -p "$live_work" "$unknown_work"
printf 'live' > "$live_work/sentinel"
printf 'unknown' > "$unknown_work/sentinel"
work_before=$(find "$LAYR_ROOT/projects/race/meta/work" -mindepth 1 -maxdepth 1 -print | sort)
mkdir -p "$T/killed-cache-barrier"
LAYR_TEST_CACHE_SAVE_BARRIER="$T/killed-cache-barrier" "$LAYR" save --caches -q > "$T/killed-cache.log" 2>&1 &
KILLED_CACHE=$!
for _ in $(seq 1 300); do
    [ -f "$T/killed-cache-barrier/ready" ] && break
    kill -0 "$KILLED_CACHE" 2>/dev/null || break
    sleep 0.1
done
if [ ! -f "$T/killed-cache-barrier/ready" ]; then
    cat "$T/killed-cache.log"; wait "$KILLED_CACHE"; echo 'FATAL: cache save did not reach the crash barrier'; exit 1
fi
kill -KILL "$KILLED_CACHE"
wait "$KILLED_CACHE" 2>/dev/null
ok "cache save succeeds after a killed save" l save --caches -q
l line new recovered main -q || exit 1
eq "the recovered cache retains the latest executable" six "$("$LAYR_ROOT/projects/race/lines/recovered/target/debug/follow-cache-race" 2>/dev/null)"
ok "cache verification succeeds after a killed save" l fsck --caches
eq "a killed cache save leaves no additional work files" "$work_before" "$(find "$LAYR_ROOT/projects/race/meta/work" -mindepth 1 -maxdepth 1 -print | sort)"
eq "a live creator's work is kept" live "$(cat "$live_work/sentinel")"
eq "work without a creator is kept" unknown "$(cat "$unknown_work/sentinel")"
ok "repair succeeds after a killed cache save" l gc --repair -q
eq "repair keeps the other work files" "$work_before" "$(find "$LAYR_ROOT/projects/race/meta/work" -mindepth 1 -maxdepth 1 -print | sort)"
rm -rf "$live_work" "$unknown_work"

finish
