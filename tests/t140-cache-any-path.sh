#!/bin/bash
# ci: macos overlayfs
# A cargo cache works at any path when no crate compiles a path of its folder in: the tests read
# CARGO_MANIFEST_DIR and CARGO_BIN_EXE_<bin> at run time (cargo and nextest set both when an integration
# test runs). A line at another path, and a clone in another store, build nothing again after they take
# the cache. `layr retest doctor`
# names each line that compiles a path in.
#
# The lines belong to the user that runs cargo: on a root run (the Linux CI job) the user who started the job.
. "$(dirname "$0")/lib.sh"
owner_toolchain_env

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
    owner_cargo $OWN
    OWN_HOME=$(getent passwd $OWN | cut -d: -f6)
    owned() { runuser -u $OWN -- env PATH=$OWN_HOME/.cargo/bin:/usr/bin:/bin "$@"; }
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

# A workspace: the package `anypath` (a binary and a test that reads both paths at run time) and its
# dependency `dep`.
cd /tmp || exit 1
SRC=$T/src
rm -rf "$SRC"; mkdir -p "$SRC/src" "$SRC/tests" "$SRC/dep/src"
printf '[package]\nname = "anypath"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\ndep = { path = "dep" }\n\n[[bin]]\nname = "x"\npath = "src/main.rs"\n\n[workspace]\n' > "$SRC/Cargo.toml"
printf '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\n' > "$SRC/dep/Cargo.toml"
echo 'pub fn one() -> i32 { 1 }' > "$SRC/dep/src/lib.rs"
echo 'fn main() { println!("{}", dep::one()); }' > "$SRC/src/main.rs"
printf '#[test]\nfn paths() {\n    println!("BIN={}", std::env::var("CARGO_BIN_EXE_x").unwrap());\n    println!("DIR={}", std::env::var("CARGO_MANIFEST_DIR").unwrap());\n}\n' > "$SRC/tests/it.rs"
echo 'target regenerable' > "$SRC/.layrattributes"
printf '/target\n' > "$SRC/.gitignore"
l init p --from "$SRC" $OWNER_ARG -q >/dev/null
P=$LAYR_ROOT/projects/p/lines

cd "$P/main" || exit 1
if [ "$(id -u)" != 0 ]; then l protect main --remove >/dev/null 2>&1 || true; fi
out=$(cb "$P/main" test --workspace -- --nocapture)
has "the test reads the binary of main" "BIN=$P/main/target/debug/x" "$out"
(cd "$P/main" && l save --overlays -q)

# A line at another path builds nothing again.
l line new $OWNER_ARG w main >/dev/null 2>&1
out=$(cb "$P/w" test --workspace -- --nocapture)
no "a line at another path compiles nothing" grep -q "Compiling" <<<"$out"
has "its test reads its own binary" "BIN=$P/w/target/debug/x" "$out"
has "its test reads its own folder" "DIR=$P/w" "$out"

# A clone in another store at another path builds nothing again: the store keeps the file times
# (docs/remote-format.md), so cargo finds the restored cache fresh. One user: a root run has no owner
# of the clone's lines here.
if [ "$(id -u)" != 0 ]; then
    l remote add archive "file://$T/archive" --paper-key-out $T/paper.txt >/dev/null 2>&1
    l push archive -q >/dev/null 2>&1
    mkdir -p $T/b
    env LAYR_ROOT=$T/b/store LAYR_PAPER_KEY_FILE=$T/paper.txt $LAYR clone "file://$T/archive" p >/dev/null 2>&1
    Q=$T/b/store/projects/p/lines
    (cd $Q/main && env LAYR_ROOT=$T/b/store LAYR_PAPER_KEY_FILE=$T/paper.txt $LAYR line new far main >/dev/null 2>&1)
    out=$(cb "$Q/far" test --workspace -- --nocapture)
    no "a line of a clone in another store compiles nothing" grep -q "Compiling" <<<"$out"
    has "its test reads its own binary" "BIN=$Q/far/target/debug/x" "$out"
fi

# doctor names a line that compiles a path in.
printf '#[test]\nfn d() {\n    let _ = env!("CARGO_MANIFEST_DIR");\n}\n' > "$P/w/tests/compiled.rs"
out=$(cd "$P/w" && owned "$LAYR" retest doctor 2>&1)
has "doctor names the line that compiles a path in" "tests/compiled.rs:3: compiles a path of this folder in" "$out"
has "doctor gives the run-time form" "read it at run time with std::env::var" "$out"
finish
