#!/bin/bash
# ci: macos overlayfs
# A warm cargo target folder names the place of its build in some units: rustc compiles in the values of
# env!("CARGO_BIN_EXE_<bin>") and env!("CARGO_MANIFEST_DIR"), and cargo does not compare them. A line that takes the
# caches of another line (`line new`, `merge --caches-only`) relocates them: these units build again and name the new
# line, the other units stay fresh.
#
# The lines belong to the user that runs cargo: on a root run (the Linux CI job) the user who started the job.
. "$(dirname "$0")/lib.sh"


if [ "$(id -u)" = 0 ]; then
    # The owner is the user who started the job (sudo sets SUDO_USER): its home holds the toolchain that cargo
    # needs. Without sudo, a test user.
    OWN=${SUDO_USER:-layr-u1}
    [ "$OWN" = root ] && OWN=layr-u1
    id $OWN >/dev/null 2>&1 || useradd -m $OWN
    chmod 755 $T $LAYR_ROOT
    # The owner runs the binary from the test folder (the checkout of the runner is not readable by the user).
    cp "$LAYR" "$T/layr" && chmod 755 "$T/layr" && LAYR=$T/layr
    # Cargo of the owner: the cargo of this machine, with a cargo home that the owner can write.
    REAL_CARGO=$(rustup which cargo 2>/dev/null || command -v cargo || true)
    [ -n "$REAL_CARGO" ] || { echo "FATAL: no cargo for the test owner (run with the PATH of the runner)" >&2; exit 1; }
    TOOLS=$(dirname "$REAL_CARGO")
    OWN_CARGO_HOME=$(mktemp -d /tmp/layr-cargo-home.XXXXXX) && chmod 777 "$OWN_CARGO_HOME"
    printf '#!/bin/sh\nexport PATH=%s:/usr/bin:/bin\nexport CARGO_HOME=%s\nexec %s "$@"\n' "$TOOLS" "$OWN_CARGO_HOME" "$REAL_CARGO" > /usr/local/bin/cargo
    chmod 755 /usr/local/bin/cargo
    trap 'rm -f /usr/local/bin/cargo; rm -rf "$OWN_CARGO_HOME"; cleanup' EXIT
    owned() { runuser -u $OWN -- env PATH=/usr/local/bin:/usr/bin:/bin "$@"; }
    OWNER_ARG="--owner $OWN"
else
    OWN=$(id -un)
    # The cargo of this machine: rustup's proxy, or the one on PATH. The runner may have no cargo on PATH.
    REAL_CARGO=$(rustup which cargo 2>/dev/null || command -v cargo || true)
    [ -n "$REAL_CARGO" ] || { echo "FATAL: no cargo found (install rustup or add its bin folder to PATH)" >&2; exit 1; }
    export PATH="$(dirname "$REAL_CARGO"):$PATH"
    owned() { "$@"; }
    OWNER_ARG=""
fi
PENV="CARGO_INCREMENTAL=1 RUSTC_WRAPPER="
# cb <folder> <cargo args...>: cargo in a line folder, as the owner of the line.
cb() { local dir=$1; shift; (cd "$dir" && owned env CARGO_NET_OFFLINE=true $PENV cargo "$@" 2>&1); }

# A workspace: the package `reloc` (a binary and a test that prints the two compiled-in paths) and its dependency
# `dep`. The standard build builds the tests.
cd /tmp || exit 1
SRC=$T/src
rm -rf "$SRC"; mkdir -p "$SRC/src" "$SRC/tests" "$SRC/dep/src"
printf '[package]\nname = "reloc"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\ndep = { path = "dep" }\n\n[[bin]]\nname = "x"\npath = "src/main.rs"\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [["test", "--workspace", "--no-run"]]\nenv = { CARGO_INCREMENTAL = "1", RUSTC_WRAPPER = "" }\n' > "$SRC/Cargo.toml"
printf '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\n' > "$SRC/dep/Cargo.toml"
echo 'pub fn one() -> i32 { 1 }' > "$SRC/dep/src/lib.rs"
echo 'fn main() { println!("{}", dep::one()); }' > "$SRC/src/main.rs"
printf '#[test]\nfn paths() {\n    println!("BIN={}", env!("CARGO_BIN_EXE_x"));\n    println!("DIR={}", env!("CARGO_MANIFEST_DIR"));\n}\n' > "$SRC/tests/it.rs"
echo 'target regenerable' > "$SRC/.layrattributes"
printf '/target\n' > "$SRC/.gitignore"
l init p --from "$SRC" $OWNER_ARG -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
new_line() { l line new $OWNER_ARG "$@"; }

cd "$P/main" || exit 1
if [ "$(id -u)" != 0 ]; then l protect main --remove >/dev/null 2>&1 || true; fi
# A line without caches yet: it gets them by a merge below.
new_line -q later main >/dev/null
out=$(cb "$P/main" test --workspace -- --nocapture)
has "the test names the binary of main" "BIN=$P/main/target/debug/x" "$out"
(cd "$P/main" && l save --caches -q)

# line new: the copied test unit builds again, the binary and the dependency do not.
err=$(new_line w main 2>&1 >/dev/null)
has "line new reports the relocated units" "units named the old path" "$err"
out=$(cb "$P/w" test --workspace -- --nocapture)
has "the test in the new line runs its own binary" "BIN=$P/w/target/debug/x" "$out"
has "the test in the new line names its own folder" "DIR=$P/w" "$out"
has "the test unit built again" "Compiling reloc" "$out"
no "the dependency stayed fresh" grep -q "Compiling dep" <<<"$out"

# merge --caches-only into a line that had no caches.
(cd "$P/later" && l merge main --caches-only -q >/dev/null 2>&1)
out=$(cb "$P/later" test --workspace -- --nocapture)
has "after a merge of caches the test runs the line's own binary" "BIN=$P/later/target/debug/x" "$out"

# A line at the place of the build has nothing to relocate.
out=$(cb "$P/main" test --workspace -- --nocapture)
no "main builds nothing again" grep -q "Compiling" <<<"$out"
finish
