#!/bin/bash
# ci: macos overlayfs
# A head move before cache publication must leave the cache unbuilt for the next pass.
. "$(dirname "$0")/lib.sh"
skip_in_check_sandbox "its cache worker runs cargo with the OS home"
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup} CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
mkdir -p "$T/home" "$T/src/src"
owner_args=""
if [ "$(id -u)" = 0 ]; then
    id layr-test >/dev/null 2>&1 || { echo "$(basename "$0") (no layr-test user to own the line): 0 passed, 0 failed"; exit 0; }
    chmod 755 "$T" "$LAYR_ROOT"
    owner_args="--owner layr-test"
    owner_cargo() { runuser -u layr-test -- env PATH="$PATH" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" CARGO_NET_OFFLINE=true cargo "$@"; }
else
    owner_cargo() { CARGO_NET_OFFLINE=true cargo "$@"; }
fi
if [ "$(uname -s)" = Darwin ]; then export HOME="$T/home"; else export LAYR_SOCKET="$T/service.sock"; fi
cat > "$T/src/Cargo.toml" <<'TOML'
[package]
name = "follow-fixture"
version = "0.1.0"
edition = "2021"
[workspace]
[workspace.metadata.layr-retest]
sweep = [["build", "--locked"]]
TOML
printf 'target/\n' > "$T/src/.gitignore"
printf 'target regenerable\n' > "$T/src/.layrattributes"
printf 'fn main() { println!("zero"); }\n' > "$T/src/src/main.rs"
(cd "$T/src" && CARGO_NET_OFFLINE=true cargo generate-lockfile >/dev/null 2>&1) || exit 1
l init p --from "$T/src" $owner_args -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/p/lines/main"
l remote add origin "file://$T/archive" --paper-key-out "$T/paper" >/dev/null || exit 1
owner_cargo build --locked -q || exit 1
l save --overlays -q || exit 1
before=$(l op log --all --json -n 1000 | wc -l | tr -d ' ')
work_before=$(find "$LAYR_ROOT/projects/p/meta/work" -mindepth 1 -maxdepth 1 -print | sort)
wrong=$(LAYR_TEST_SAVE_EXPECTED_HEAD=not-the-head l save --overlays -q 2>&1); wrong_rc=$?
ok "a cache save refuses a different expected head" test "$wrong_rc" -ne 0
has "the refusal names the moved line" "moved before the cache save" "$wrong"
eq "the refused save writes no record" "$before" "$(l op log --all --json -n 1000 | wc -l | tr -d ' ')"
eq "the refused save leaves no prepared work" "$work_before" "$(find "$LAYR_ROOT/projects/p/meta/work" -mindepth 1 -maxdepth 1 -print | sort)"
l push origin -q || exit 1
l follow origin/main >/dev/null 2>&1 || exit 1
printf 'fn main() { println!("first"); }\n' > src/main.rs
l add src/main.rs && l save -m first -q || exit 1
LAYR_TEST_FOLLOW_BEFORE_PUBLISH_PAUSE_MS=20000 CARGO_NET_OFFLINE=true l follow --run > "$T/follow.out" 2>&1 &
pid=$!
for i in $(seq 1 600); do
    grep -q 'test pause before follow cache publish' "$T/follow.out" && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
done
if ! grep -q 'test pause before follow cache publish' "$T/follow.out"; then
    cat "$T/follow.out"; wait "$pid" || true; exit 1
fi
printf 'fn main() { println!("second"); }\n' > src/main.rs
l add src/main.rs && l save -m second -q || { wait "$pid" || true; exit 1; }
new_head=$(l rev-parse head)
wait "$pid"; rc=$?
eq "follow finishes after the head moves" 0 "$rc"
has "follow does not publish the stale cache" "caches not saved" "$(cat "$T/follow.out")"
eq "the saved head is the new head" "$new_head" "$(l rev-parse head)"
ok "the stale build leaves an unbuilt mark" sh -c 'find "$1" -name "*.unbuilt" | grep -q .' _ "$LAYR_ROOT/projects/p/meta/line-locks"
retry=$(CARGO_NET_OFFLINE=true l follow --run 2>&1)
has "the next pass rebuilds and saves" "caches saved with the head" "$retry"
eq "the cache has the new executable" "second" "$(target/debug/follow-fixture)"
no "the completed pass leaves no unbuilt mark" sh -c 'find "$1" -name "*.unbuilt" | grep -q .' _ "$LAYR_ROOT/projects/p/meta/line-locks"
l line new after $owner_args -q >/dev/null || exit 1
eq "a new line takes the cache for the new head" "second" "$($LAYR_ROOT/projects/p/lines/after/target/debug/follow-fixture)"
finish
