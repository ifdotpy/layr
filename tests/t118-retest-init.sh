#!/bin/bash
# ci: macos overlayfs
# `layr retest init` alone sets up a fresh cargo project with a git remote (docs/retest.md, Start). The main line
# follows origin/main, the sweep list covers the standard commands, and the caches are warm and thin. Checks: a new
# line from main compiles no crate on its first build; a change of a line, saved with its caches and merged into
# main, makes main compile the changed crate only; 20 saves with builds in a line grow the store by about the
# tracked changes and one live cache; `layr retest doctor` reports no project problem.
#
# The owner runs the layr commands. On a root run (the Linux CI job) the lines belong to a test user, because the
# cache hook never prunes a root-owned line (as t117 does).
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
    REAL_CARGO=$(rustup which cargo 2>/dev/null || command -v cargo || true)
    [ -n "$REAL_CARGO" ] || { echo "FATAL: no cargo for the test owner (run with the PATH of the runner)" >&2; exit 1; }
    TOOLS=$(dirname "$REAL_CARGO")
    OWN_CARGO_HOME=$(mktemp -d /tmp/layr-cargo-home.XXXXXX) && chmod 777 "$OWN_CARGO_HOME"
    trap 'rm -rf "$OWN_CARGO_HOME"; cleanup' EXIT
    owned() { runuser -u $OWN -- env PATH=$TOOLS:/usr/bin:/bin CARGO_HOME=$OWN_CARGO_HOME "$@"; }
    CARGO=$REAL_CARGO
else
    OWN=$(id -un)
    owned() { "$@"; }
    CARGO=cargo
fi
# ly: layr as the owner. The owner's own store commands need no root.
ly() { owned "$LAYR" "$@"; }
# PENV: the project env of the metadata that init writes (`[workspace.metadata.layr-retest] env`). Every cargo call
# uses it, so the standard units are the ones the sweep keeps.
PENV="CARGO_INCREMENTAL=1 RUSTC_WRAPPER="
# cb <folder> <cargo args...>: cargo in a line folder, as the owner, with the project env. Prints the output.
cb() { local dir=$1; shift; (cd "$dir" && owned env CARGO_NET_OFFLINE=true $PENV $CARGO "$@" 2>&1); }
# std <folder>: the three standard commands of init (the debug build, the test build, the release build).
std() { cb "$1" build --locked -q >/dev/null; cb "$1" test --locked --no-run -q >/dev/null; cb "$1" build --release --locked -q >/dev/null; }
# compiled <folder> <cargo args...>: the number of crates a command compiles.
compiled() { local d=$1; shift; cb "$d" "$@" | grep -c '^ *Compiling '; }
units() { ls "$1/debug/.fingerprint" 2>/dev/null | wc -l | tr -d ' '; }
# store <bytes>: the total bytes of the store (layr du --store --json), exact (APFS clones are not double counted).
store() { ly du --store --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["total"])'; }

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
(cd $U && CARGO_NET_OFFLINE=true $CARGO generate-lockfile) || exit 1
git add -A && git commit -qm base && git push -q origin main

# The project: one command, `layr retest init`, in its main line.
ly git clone $R proj -q >/dev/null
P=$LAYR_ROOT/projects/proj/lines
cd $P/main || exit 1
ly protect main --remove >/dev/null 2>&1 || true
init_out=$(ly retest init 2>&1); init_rc=$?
eq "init exits 0" 0 "$init_rc"
has "init sets the follow target of main" "follow: main follows origin/main" "$init_out"
has "the follow target is in layr follow" "Following origin/main on line main" "$(ly follow 2>&1)"
has "the sweep list covers the three standard commands" 'sweep = [["build", "--locked"], ["test", "--locked", "--no-run"], ["build", "--release", "--locked"]]' "$(cat Cargo.toml)"
eq "init adds no cache before a build" "" "$(ly overlay list 2>&1 | grep cache.target)"

# A second init changes nothing and leaves the follow target alone.
before_files=$(cat Cargo.toml .layrattributes .gitignore | cksum)
second=$(ly retest init 2>&1)
has "a second init keeps the follow target" "follow: main already follows origin/main" "$second"
eq "a second init changes no file" "$before_files" "$(cat Cargo.toml .layrattributes .gitignore | cksum)"

# The files that init changed are saved once: a line takes them from a state (the printed "save" step).
ly add .layrattributes Cargo.toml .gitignore >/dev/null 2>&1
ly save -m "retest init" >/dev/null 2>&1 || fail "the save of the init files failed"
# The first build, then a save: the save makes target a cache overlay with no save --caches (#178).
std $P/main
ly save >/dev/null 2>&1 || fail "the save after the first build failed"
has "the save after the first build makes target a cache overlay" "cache.target" "$(ly overlay list)"
base_units=$(units $P/main/target)
has "the main line builds with the standard env" "Finished" "$(cb $P/main build --locked 2>&1)"

# 1. A new line from main compiles no crate on its first build.
ly line new t1 main >/dev/null 2>&1
n=$(compiled $P/t1 build --locked)
eq "a new line from main compiles 0 crates on its first build" 0 "$n"
n=$(compiled $P/t1 test --locked --no-run)
eq "a new line from main compiles 0 test crates" 0 "$n"

# 2. A change in the line, saved with its caches, merged into main. Main compiles the changed crate only.
echo 'pub fn extra() -> i32 {' > $P/t1/crates/b/src/extra.rs && echo '    7' >> $P/t1/crates/b/src/extra.rs && echo '}' >> $P/t1/crates/b/src/extra.rs
echo 'mod extra;' >> $P/t1/crates/b/src/lib.rs && echo 'pub use extra::extra;' >> $P/t1/crates/b/src/lib.rs
(cd $P/t1 && ly add crates/b/src/extra.rs crates/b/src/lib.rs >/dev/null && ly save -m "b: extra" >/dev/null)
std $P/t1
(cd $P/t1 && ly save --caches >/dev/null 2>&1)
merged=$(cd $P/t1 && ly rev-parse head)
cd $P/main && ly merge t1 --expect "$merged" >/dev/null 2>&1 || fail "the merge of t1 into main failed"
out=$(cb $P/main build --locked)
n=$(echo "$out" | grep -c '^ *Compiling ')
eq "main compiles the changed crate only (one crate)" 1 "$n"
has "main compiles crate b" "Compiling b " "$out"
std $P/main
(cd $P/main && ly save --caches >/dev/null 2>&1)
eq "main has no unit of the line's change beyond the standard set" "$base_units" "$(units $P/main/target)"
# The sessions of incremental builds: a changed crate leaves an older session behind until a prune. After the save
# of main, each crate folder keeps its newest session only.
sessions() { find "$1/debug/incremental" -mindepth 2 -maxdepth 2 -type d -name 's-*' 2>/dev/null | wc -l | tr -d ' '; }
folders() { ls "$1/debug/incremental" 2>/dev/null | wc -l | tr -d ' '; }
eq "main keeps one incremental session per crate folder" "$(folders $P/main/target)" "$(sessions $P/main/target)"

# 3. Twenty saves with builds in a line: the store grows by the tracked changes and one live cache, not 20 copies.
ly line new t2 main >/dev/null 2>&1
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
std $P/main
doctor=$(ly retest doctor 2>&1)
no "doctor reports no prune warning" sh -c 'echo "$1" | grep -q "warn: prune"' _ "$doctor"
no "doctor reports no shell env warning" sh -c 'echo "$1" | grep -q "warn: shell"' _ "$doctor"

finish
