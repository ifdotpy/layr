#!/bin/bash
# ci: macos overlayfs
# `layr retest init` alone sets up a fresh cargo project with a git remote (docs/retest.md, Start). The main line
# follows origin/main, the sweep list covers the standard commands, and the caches are warm and thin. The main line
# stays protected: the init files come in through a merge, as in a real project. Checks: a new line from main compiles
# no crate on its first build; a change of a line, saved with its caches and merged into main, makes main compile the
# changed crate only; 20 saves with builds in a line grow the store by about the tracked changes and one live cache;
# `layr retest doctor` reports no project problem.
#
# The layr commands run as the test's user (root on the Linux CI job, as in t117). The cargo commands run as the
# owner of the lines, so the sweep and the cache hook see the owner's units. On a root run the owner's cargo is the
# toolchain of the runner, found through the runner's rustup home (root has no toolchain of its own).
. "$(dirname "$0")/lib.sh"
fail() { FAIL=$((FAIL + 1)); echo "FAIL $*"; }
# layr follow probes the user socket (macOS: under HOME) or LAYR_SOCKET: keep both in the test folder. Cargo and
# rustup keep their homes from the runner.
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup} CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
mkdir -p $T/home
if [ "$(uname -s)" = Darwin ]; then export HOME=$T/home; else export LAYR_SOCKET=$T/service.sock; fi

if [ "$(id -u)" = 0 ]; then
    OWN=${SUDO_USER:-layr-u1}
    [ "$OWN" = root ] && OWN=layr-u1
    id $OWN >/dev/null 2>&1 || useradd -m $OWN
    chmod 755 $T $LAYR_ROOT
    cp "$LAYR" "$T/layr" && chmod 755 "$T/layr" && LAYR=$T/layr
    if [ -n "${SUDO_USER:-}" ]; then
        SUDO_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
        export RUSTUP_HOME=${SUDO_RUSTUP_HOME:-$SUDO_HOME/.rustup} CARGO_HOME=${SUDO_CARGO_HOME:-$SUDO_HOME/.cargo}
    fi
    REAL_CARGO=$(rustup which cargo 2>/dev/null || command -v cargo || true)
    [ -n "$REAL_CARGO" ] || { echo "FATAL: no cargo for the test (run with the PATH of the runner)" >&2; exit 1; }
    TOOLS=$(dirname "$REAL_CARGO")
    export PATH=$TOOLS:$PATH
    OWN_CARGO_HOME=$(mktemp -d /tmp/layr-cargo-home.XXXXXX) && chmod 777 "$OWN_CARGO_HOME"
    trap 'rm -rf "$OWN_CARGO_HOME"; cleanup' EXIT
    owned() { runuser -u $OWN -- env PATH=$TOOLS:/usr/bin:/bin CARGO_HOME=$OWN_CARGO_HOME "$@"; }
    CARGO=$REAL_CARGO
    OWNER_ARG="--owner $OWN"
else
    OWN=$(id -un)
    owned() { "$@"; }
    CARGO=cargo
    OWNER_ARG=""
fi
# ly: layr as the test's user (root on a root run, as t117).
ly() { "$LAYR" "$@"; }
# PENV: the project env of the metadata that init writes (`[workspace.metadata.layr-retest] env`). Every cargo call
# uses it, so the standard units are the ones the sweep keeps.
PENV="CARGO_INCREMENTAL=1 RUSTC_WRAPPER="
# cb <folder> <cargo args...>: cargo in a line folder, as the owner, with the project env. Prints the output.
cb() { local dir=$1; shift; (cd "$dir" && owned env CARGO_NET_OFFLINE=true $PENV $CARGO "$@" 2>&1); }
# std <folder>: the three standard commands of init (the debug build, the test build, the release build).
std() { cb "$1" build --locked -q >/dev/null; cb "$1" test --locked --no-run -q >/dev/null; cb "$1" build --release --locked -q >/dev/null; }
# compiled <folder> <cargo args...>: the number of crates a command compiles.
compiled() { local d=$1; shift; cb "$d" "$@" | grep -c '^ *Compiling '; }
# store: the total bytes of the store (layr du --store --json), exact (APFS clones are not double counted).
store() { ly du --store --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["total"])'; }
# The incremental sessions of a target folder: one session folder per crate folder after a prune.
sessions() { find "$1/debug/incremental" -mindepth 2 -maxdepth 2 -type d -name 's-*' 2>/dev/null | wc -l | tr -d ' '; }
folders() { ls "$1/debug/incremental" 2>/dev/null | wc -l | tr -d ' '; }

# The remote: a fresh cargo workspace of two crates. Cargo.lock is committed, so `--locked` works.
R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null
cd $U || exit 1
mkdir -p crates/a/src crates/b/src
printf '[workspace]\nmembers = ["crates/a", "crates/b"]\nresolver = "2"\n' > Cargo.toml
printf '[package]\nname = "a"\nversion = "0.1.0"\nedition = "2021"\n' > crates/a/Cargo.toml
printf 'pub fn a() -> i32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a_is_one() {\n        assert_eq!(super::a(), 1);\n    }\n}\n' > crates/a/src/lib.rs
printf '[package]\nname = "b"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\na = { path = "../a" }\n' > crates/b/Cargo.toml
printf 'pub fn b() -> i32 {\n    a::a() + 1\n}\n' > crates/b/src/lib.rs
printf 'target/\n' > .gitignore
(CARGO_NET_OFFLINE=true $CARGO generate-lockfile) || exit 1
git add -A && git commit -qm base && git push -q origin main

# The project, from the git remote (its main line is protected, as in a real project).
ly init proj --git $R $OWNER_ARG -q >/dev/null 2>&1 || { echo "FATAL: layr init --git failed" >&2; exit 1; }
P=$LAYR_ROOT/projects/proj/lines

# The init files come in through a line and a merge: main is protected.
(cd $P/main && ly line new setup main $OWNER_ARG >/dev/null 2>&1)
cd $P/setup || exit 1
init_out=$(ly retest init 2>&1); init_rc=$?
# Missing cargo-nextest or llvm-tools (not on every runner) is reported by init with exit 1; the files are still changed.
if [ "$init_rc" -eq 0 ] || echo "$init_out" | grep -q '^missing '; then PASS=$((PASS + 1)); echo "PASS init runs (exit $init_rc: 0, or only missing tools reported)"; else fail "init failed: $init_out"; fi
ly add .layrattributes Cargo.toml .gitignore >/dev/null 2>&1
ly save -m "retest init" >/dev/null 2>&1 || fail "the save of the init files failed"
setup_head=$(ly rev-parse head)
cd $P/main || exit 1
ly merge setup --expect "$setup_head" >/dev/null 2>&1 || fail "the merge of the init files into main failed"

# In main: the follow target, the sweep list, and a second init that changes nothing.
main_init=$(ly retest init 2>&1); main_rc=$?
has "init in main sets the follow target" "follow: main follows origin/main" "$main_init"
has "the follow target is in layr follow" "Following origin/main on line main" "$(ly follow 2>&1)"
has "the sweep list covers the three standard commands" 'sweep = [["build", "--locked"], ["test", "--locked", "--no-run"], ["build", "--release", "--locked"]]' "$(cat Cargo.toml)"
before_files=$(cat Cargo.toml .layrattributes .gitignore | cksum)
second=$(ly retest init 2>&1)
has "a second init keeps the follow target" "follow: main already follows origin/main" "$second"
eq "a second init changes no file" "$before_files" "$(cat Cargo.toml .layrattributes .gitignore | cksum)"

# The caches come from main, which stays protected. A cache that a root run saves belongs to root, and other users take
# it only from a protected line (t117). So main builds, then its caches are saved.
std $P/main
(cd $P/main && ly save --caches >/dev/null 2>&1) || fail "save --caches of main failed"
has "save --caches of main makes target a cache overlay" "cache.target" "$(ly overlay list)"

# 1. A new line from main compiles no crate on its first build.
(cd $P/main && ly line new t1 main $OWNER_ARG >/dev/null 2>&1)
n=$(compiled $P/t1 build --locked)
eq "a new line from main compiles 0 crates on its first build" 0 "$n"
n=$(compiled $P/t1 test --locked --no-run)
eq "a new line from main compiles 0 test crates" 0 "$n"

# 2. A change in the line, saved with its caches, merged into main. Main compiles the changed crate only.
echo 'pub fn extra() -> i32 {' > $P/t1/crates/b/src/extra.rs && echo '    7' >> $P/t1/crates/b/src/extra.rs && echo '}' >> $P/t1/crates/b/src/extra.rs
echo 'mod extra;' >> $P/t1/crates/b/src/lib.rs && echo 'pub use extra::extra;' >> $P/t1/crates/b/src/lib.rs
(cd $P/t1 && ly add crates/b/src/extra.rs crates/b/src/lib.rs >/dev/null 2>&1 && ly save -m "b: extra" >/dev/null 2>&1) || fail "the save of the change failed"
std $P/t1
(cd $P/t1 && ly save --caches >/dev/null 2>&1) || fail "save --caches of the line failed"
merged=$(cd $P/t1 && ly rev-parse head)
cd $P/main && ly merge t1 --expect "$merged" >/dev/null 2>&1 || fail "the merge of t1 into main failed"
out=$(cb $P/main build --locked)
n=$(echo "$out" | grep -c '^ *Compiling ')
eq "main compiles the changed crate only (one crate)" 1 "$n"
has "main compiles crate b" "Compiling b " "$out"
std $P/main
(cd $P/main && ly save --caches >/dev/null 2>&1) || fail "save --caches of main after the merge failed"
# After save --caches, the prune leaves each incremental crate folder one session.
eq "main keeps one incremental session per crate folder after its save" "$(folders $P/main/target)" "$(sessions $P/main/target)"

# 3. Twenty saves with builds in a line: the store grows by the tracked changes and one live cache, not 20 copies.
(cd $P/main && ly line new t2 main $OWNER_ARG >/dev/null 2>&1)
cd $P/t2 || exit 1
s0=$(store)
for i in $(seq 1 20); do
    echo "pub fn gen$i() -> i32 {" >> crates/a/src/lib.rs && echo "    $i" >> crates/a/src/lib.rs && echo "}" >> crates/a/src/lib.rs
    ly save -a -m "iter $i" >/dev/null 2>&1 || fail "save $i failed"
    std $P/t2
done
s1=$(store)
live=$(du -sk $P/t2/target | awk '{print $1 * 1024}')
grow=$((s1 - s0))
echo "store growth over 20 saves: $grow bytes; one live cache of the line: $live bytes"
[ "$grow" -lt $((2 * live + 1048576)) ] && { PASS=$((PASS + 1)); echo "PASS the store grows by about one live cache, not 20"; } || { FAIL=$((FAIL + 1)); echo "FAIL the store grew $grow bytes for 20 saves (one live cache is $live bytes)"; }

# 4. The project is healthy: no project warning from doctor.
cd $P/main || exit 1
doctor=$(ly retest doctor 2>&1)
no "doctor reports no prune warning" sh -c 'echo "$1" | grep -q "warn: prune"' _ "$doctor"
no "doctor reports no shell env warning" sh -c 'echo "$1" | grep -q "warn: shell"' _ "$doctor"

finish
