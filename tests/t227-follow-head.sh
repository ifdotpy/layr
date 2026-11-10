#!/bin/bash
# ci: macos overlayfs
# A local save after a follow build must cause a rebuild for the new head.
. "$(dirname "$0")/lib.sh"
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup} CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
mkdir -p "$T/home" "$T/src/src"
owner_args=""
if [ "$(id -u)" = 0 ]; then
    id layr-test >/dev/null 2>&1 || { echo "FATAL: missing layr-test user" >&2; exit 1; }
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
l save --caches -q || exit 1
l push origin -q || exit 1
l follow origin/main >/dev/null 2>&1 || exit 1
printf 'fn main() { println!("first"); }\n' > src/main.rs
l add src/main.rs && l save -m first -q || exit 1
LAYR_TEST_FOLLOW_AFTER_BUILD_PAUSE_MS=20000 CARGO_NET_OFFLINE=true l follow --run > "$T/follow.out" 2>&1 &
pid=$!
for i in $(seq 1 600); do
    grep -q 'test pause after follow build' "$T/follow.out" && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
done
if ! grep -q 'test pause after follow build' "$T/follow.out"; then
    cat "$T/follow.out"; wait "$pid" || true; exit 1
fi
printf 'fn main() { println!("second"); }\n' > src/main.rs
l add src/main.rs && l save -m second -q || { wait "$pid" || true; exit 1; }
new_head=$(l rev-parse head)
wait "$pid"; rc=$?
eq "follow finishes after the head moves" 0 "$rc"
has "follow rebuilds for the new head" "during the cache build; building again" "$(cat "$T/follow.out")"
eq "the saved head is the new head" "$new_head" "$(l rev-parse head)"
eq "the cache has the new executable" "second" "$(target/debug/follow-fixture)"
no "the completed pass leaves no unbuilt mark" sh -c 'find "$1" -name "*.unbuilt" | grep -q .' _ "$LAYR_ROOT/projects/p/meta/line-locks"
l line new after $owner_args -q >/dev/null || exit 1
eq "a new line takes the cache for the new head" "second" "$($LAYR_ROOT/projects/p/lines/after/target/debug/follow-fixture)"
out=$(l follow --run 2>&1)
no "the next pass does not build again" grep -q 'caches saved with the head' <<<"$out"
finish
