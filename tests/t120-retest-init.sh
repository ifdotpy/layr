#!/bin/bash
# ci: macos overlayfs
# `layr retest init` alone sets up a fresh cargo project with a git remote (docs/retest.md, Start). The main line
# follows origin/main, the sweep list covers the standard commands, and the caches are warm and thin. Main stays
# protected: the init files come in through a merge, a change of a line lands on the git remote as a squash merge and
# the real follow pass takes it, and a local merge into main takes and saves the line's caches (as the follow pass
# does). Checks: a new line from main compiles no crate after the follow pass of a code change and after a local merge
# (the caches carry the source file times: docs/cache-overlays.md, section 20), a file changed after the cache save
# is built again; 20 saves with builds in a line
# grow the store by about the tracked changes and one live cache; `layr retest doctor` reports no project problem.
#
# The layr commands run as the test's user (root on the Linux CI job, as in t117). The cargo commands run as the
# owner of the lines, so the sweep and the cache hook see the owner's units. On a root run the owner's cargo is the
# toolchain of the runner, found through the runner's rustup home.
. "$(dirname "$0")/lib.sh"
skip_in_check_sandbox "its cache worker runs cargo with the OS home"
fail() { FAIL=$((FAIL + 1)); echo "FAIL $*"; }
# layr follow probes the user socket (macOS: under HOME) or LAYR_SOCKET: keep both in the test folder. Cargo and
# rustup keep their homes from the runner.
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup} CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
owner_toolchain_env
mkdir -p $T/home
if [ "$(uname -s)" = Darwin ]; then export HOME=$T/home; else export LAYR_SOCKET=$T/service.sock; fi

if [ "$(id -u)" = 0 ]; then
    OWN=${SUDO_USER:-layr-u1}
    [ "$OWN" = root ] && OWN=layr-u1
    id $OWN >/dev/null 2>&1 || useradd -m $OWN
    chmod 755 $T $LAYR_ROOT
    cp "$LAYR" "$T/layr" && chmod 755 "$T/layr" && LAYR=$T/layr
    REAL_CARGO=$(rustup which cargo 2>/dev/null || command -v cargo || true)
    [ -n "$REAL_CARGO" ] || { echo "FATAL: no cargo for the test (run with the PATH of the runner)" >&2; exit 1; }
    TOOLS=$(dirname "$REAL_CARGO")
    export PATH=$TOOLS:$PATH
    owner_cargo $OWN
    OWN_HOME=$(getent passwd $OWN | cut -d: -f6)
    owned() { runuser -u $OWN -- env PATH=$OWN_HOME/.cargo/bin:$TOOLS:/usr/bin:/bin CARGO_HOME=$OWN_HOME/.cargo "$@"; }
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
# compiled <folder> <cargo args...>: the number of crates a command compiles. -1 when cargo did not finish (a missing folder).
compiled() {
    local d=$1 out; shift
    out=$(cb "$d" "$@")
    echo "$out" | grep -q '^ *Finished' || { echo -1; return; }
    echo "$out" | grep -c '^ *Compiling '
}
# store: the total bytes of the store (layr du --store --json), exact (APFS clones are not double counted).
store() { ly du --store --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["total"])'; }
# parts: the bytes each store part frees (`layr du --store --json`), for the diagnosis of a store growth.
parts() { ly du --store --json | python3 -c 'import json,sys; [print(p["part"] + "\t" + str(p["frees"])) for p in json.load(sys.stdin)["parts"]]'; }
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

# In protected main before the merge, init changes no file and says how to bring the files in.
before_main=$(cd $P/main && cat Cargo.toml .gitignore .layrattributes 2>/dev/null | cksum)
guard=$(cd $P/main && ly retest init 2>&1); guard_rc=$?
has "init in a protected main names the way through a line" "is protected, so init changed no file" "$guard"
eq "init in a protected main exits 1 while files need the change" 1 "$guard_rc"
eq "init in a protected main writes no file" "$before_main" "$(cd $P/main && cat Cargo.toml .gitignore .layrattributes 2>/dev/null | cksum)"
has "init in main sets the follow target" "follow: main follows origin/main" "$guard"

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
# The remote has the init files too: the follow pass of main then sees a fast-forward.
(cd $P/main && ly git push origin main >/dev/null 2>&1) || fail "push of main to the remote failed"

# In main: the follow target, the sweep list, and a second init that changes nothing.
main_init=$(ly retest init 2>&1)
has "init in main after the merge keeps the follow target" "follow: main already follows origin/main" "$main_init"
has "the follow target is in layr follow" "Following origin/main on line main" "$(ly follow 2>&1)"
has "the sweep list covers the three standard commands" 'sweep = [["build", "--locked"], ["test", "--locked", "--no-run"], ["build", "--release", "--locked"]]' "$(cat Cargo.toml)"
before_files=$(cat Cargo.toml .layrattributes .gitignore | cksum)
second=$(ly retest init 2>&1)
has "a second init keeps the follow target" "follow: main already follows origin/main" "$second"
eq "a second init changes no file" "$before_files" "$(cat Cargo.toml .layrattributes .gitignore | cksum)"

# 1. The remote flow. A line changes crate b, saves with its caches and pushes its branch (the export that the follow
#    pass reads). The change lands on the remote as a squash merge. The follow pass of main runs as the daemon runs it.
(cd $P/main && ly line new t2 main $OWNER_ARG >/dev/null 2>&1)
std $P/t2 >/dev/null
echo 'pub fn extra() -> i32 {' > $P/t2/crates/b/src/extra.rs && echo '    7' >> $P/t2/crates/b/src/extra.rs && echo '}' >> $P/t2/crates/b/src/extra.rs
echo 'mod extra;' >> $P/t2/crates/b/src/lib.rs && echo 'pub use extra::extra;' >> $P/t2/crates/b/src/lib.rs
(cd $P/t2 && ly add crates/b/src/extra.rs crates/b/src/lib.rs >/dev/null 2>&1 && ly save -m "b: extra" >/dev/null 2>&1) || fail "the save of the change failed"
std $P/t2
(cd $P/t2 && ly save --glasses >/dev/null 2>&1) || fail "save --glasses of t2 failed"
(cd $P/t2 && ly git push origin t2 >/dev/null 2>&1) || fail "push of t2 to the remote failed"
cd $U && git pull -q origin main
cp $P/t2/crates/b/src/extra.rs $P/t2/crates/b/src/lib.rs crates/b/src/ && git add crates/b/src && git commit -qm "b: extra (squash of t2)" && git push -q origin main || fail "the squash merge on the remote failed"
(cd $P/main && ly follow --tick >/dev/null 2>&1)
has "the follow pass of main fast-forwards to the squash merge" "fast-forward to" "$(cd $P/main && ly follow 2>&1)"
[ -f $P/main/crates/b/src/extra.rs ] && { PASS=$((PASS + 1)); echo "PASS main has the squash merge of t2"; } || fail "main has no extra.rs after the follow pass"
(cd $P/main && ly line new t3 main $OWNER_ARG >/dev/null 2>&1)
n=$(compiled $P/t3 build --locked)
eq "a new line from main compiles 0 crates after the follow pass (a code change)" 0 "$n"

# 1b. A change that comes without a line (a merge on the remote of a branch that no line here pushed): the follow pass
#     builds the standard commands in main before it saves the caches, so a new line from main compiles 0 crates.
cd $U && git pull -q origin main
printf 'pub fn a() -> i32 {\n    2 - 1\n}\n' > crates/a/src/lib.rs && git add crates/a/src && git commit -qm "a: from the remote" && git push -q origin main || fail "the remote change failed"
tick=$(cd $P/main && ly follow --tick 2>&1)
echo "$tick" | grep -i "build\|prune" | sed 's/^/FOLLOW: /'
has "the follow pass builds the cache folder" "built with cargo" "$tick"
(cd $P/main && ly line new t5 main $OWNER_ARG >/dev/null 2>&1)
eq "after a remote change without a line, a new line from main compiles 0 crates" 0 "$(compiled $P/t5 build --locked)"
eq "and 0 test crates" 0 "$(compiled $P/t5 test --locked --no-run)"

# 1c. A build that does not finish (a compile error on the remote) leaves main marked: the next pass builds again,
#     and after the fix the pass after a good build does nothing.
cd $U && git pull -q origin main
printf 'pub fn a() -> i32 {\n    not_here()\n}\n' > crates/a/src/lib.rs && git add crates/a/src && git commit -qm "a: broken" && git push -q origin main || fail "the broken remote change failed"
has "the pass of a broken change does not build" "not built with cargo" "$(cd $P/main && ly follow --tick 2>&1)"
(cd $P/main && ly follow --tick >/dev/null 2>&1)
# A build that did not finish saves no cache for the head: main stays marked and the next pass builds again.
has "the next pass builds again" "cache build did not finish for the current head; the next follow pass will retry" "$(cd $P/main && ly follow 2>&1)"
cd $U && printf 'pub fn a() -> i32 {\n    2 - 1\n}\n' > crates/a/src/lib.rs && git add crates/a/src && git commit -qm "a: fixed" && git push -q origin main || fail "the fix on the remote failed"
has "the pass of the fix builds" "built with cargo" "$(cd $P/main && ly follow --tick 2>&1)"
(cd $P/main && ly follow --tick >/dev/null 2>&1)
no "the pass after a good build does nothing" sh -c 'echo "$1" | grep -q "caches saved"' _ "$(cd $P/main && ly follow 2>&1)"

# 2. A local merge into protected main takes and saves the caches of the line (as the follow pass does). A docs change
#    keeps every crate file, so a new line from main compiles 0 crates.
(cd $P/main && ly line new w main $OWNER_ARG >/dev/null 2>&1)
std $P/w >/dev/null
echo 'Notes of the test line.' > $P/w/NOTES.md
(cd $P/w && ly add NOTES.md >/dev/null 2>&1 && ly save -m "notes" >/dev/null 2>&1) || fail "the save of NOTES.md failed"
(cd $P/w && ly save --glasses >/dev/null 2>&1) || fail "save --glasses of w failed"
wh=$(cd $P/w && ly rev-parse head)
cd $P/main && out=$(LAYR_TRACE=1 ly merge w --expect "$wh" 2>&1) || fail "the local merge of w into main failed"
[ -n "$out" ] && echo "$out" | grep -iE "cache" | sed "s/^/MERGE TRACE: /" | head -30
echo "$out" | sed 's/^/MERGE OUTPUT: /'
has "a local merge reports the caches it takes" "Merged cache target" "$out"
(cd $P/main && ly line new t1 main $OWNER_ARG >/dev/null 2>&1)
n=$(compiled $P/t1 build --locked)
eq "after a local merge of a docs change, a new line from main compiles 0 crates" 0 "$n"
n=$(compiled $P/t1 test --locked --no-run)
eq "a new line from main compiles 0 test crates" 0 "$n"

# 2b. Negative: a crate file changed after the caches were saved keeps its new time, so cargo rebuilds crate a for a
#     line from that change. The caches are the project's; their project state is not the change.
(cd $P/main && ly line new neg main $OWNER_ARG >/dev/null 2>&1)
echo 'pub fn changed() -> i32 {' >> $P/neg/crates/a/src/lib.rs && echo '    3' >> $P/neg/crates/a/src/lib.rs && echo '}' >> $P/neg/crates/a/src/lib.rs
(cd $P/neg && ly save -a -m "a: changed after the caches" >/dev/null 2>&1) || fail "the save of the change failed"
(cd $P/main && ly line new negl neg $OWNER_ARG >/dev/null 2>&1)
out=$(cb $P/negl build --locked)
has "a line from a change after the cache save compiles crate a (the time of the write is kept)" "Compiling a " "$out"

# 2c. Two caches saved at other times: a line saves both caches, then changes crate a and saves only target. A line
#     from it takes target (it saw the change) and the side cache (it saw the old bytes); the file gets the time that
#     target recorded, so it compiles 0 crates (docs/cache-overlays.md, section 20).
(cd $P/main && ly line new two main $OWNER_ARG >/dev/null 2>&1)
cd $P/two || exit 1
printf 'side/\n' >> .gitignore && printf 'side regenerable\n' >> .layrattributes
mkdir -p side && echo built > side/out
std $P/two >/dev/null
(ly add .gitignore .layrattributes >/dev/null 2>&1 && ly save -m "a side cache" >/dev/null 2>&1) || fail "the save of the side cache rule failed"
ly save --glasses >/dev/null 2>&1 || fail "save --glasses of two failed"
echo 'pub fn later() -> i32 {' >> crates/a/src/lib.rs && echo '    4' >> crates/a/src/lib.rs && echo '}' >> crates/a/src/lib.rs
std $P/two >/dev/null
(ly save -a -m "a: changed after the side save" >/dev/null 2>&1) || fail "the save of the change in two failed"
ly save --glasses --folder target >/dev/null 2>&1 || fail "save --glasses --folder target of two failed"
(cd $P/main && ly line new twol two $OWNER_ARG >/dev/null 2>&1)
eq "a line from a line whose caches were saved at other times compiles 0 crates" 0 "$(compiled $P/twol build --locked)"

# 3. Twenty saves with builds in a line: the store grows by the tracked changes and one live cache, not 20 copies.
(cd $P/main && ly line new t4 main $OWNER_ARG >/dev/null 2>&1)
cd $P/t4 || exit 1
s0=$(store)
parts > $T/parts0.txt
for i in $(seq 1 20); do
    echo "pub fn gen$i() -> i32 {" >> crates/a/src/lib.rs && echo "    $i" >> crates/a/src/lib.rs && echo "}" >> crates/a/src/lib.rs
    ly save -a -m "iter $i" >/dev/null 2>&1 || fail "save $i failed"
    std $P/t4
done
s1=$(store)
parts > $T/parts1.txt
live=$(du -sk $P/t4/target | awk '{print $1 * 1024}')
grow=$((s1 - s0))
echo "store growth over 20 saves: $grow bytes; one live cache of the line: $live bytes"
echo "STORE PARTS (bytes per part, before and after the 20 saves):"
python3 - $T/parts0.txt $T/parts1.txt <<'PY'
import sys
a = dict(l.rstrip('\n').split('\t') for l in open(sys.argv[1]) if l.strip())
b = dict(l.rstrip('\n').split('\t') for l in open(sys.argv[2]) if l.strip())
for k in sorted(set(a) | set(b)):
    d = int(b.get(k, 0)) - int(a.get(k, 0))
    if d: print(f"STORE PART {k} before {a.get(k, 0)} after {b.get(k, 0)} delta {d}")
PY
[ "$grow" -lt $((2 * live + 1048576)) ] && { PASS=$((PASS + 1)); echo "PASS the store grows by about one live cache, not 20"; } || { FAIL=$((FAIL + 1)); echo "FAIL the store grew $grow bytes for 20 saves (one live cache is $live bytes)"; }

# 4. The project is healthy: no project warning from doctor.
cd $P/t4 || exit 1
doctor=$(ly retest doctor 2>&1)
echo "$doctor" | sed 's/^/DOCTOR: /'
no "doctor reports no prune warning" sh -c 'echo "$1" | grep -q "warn: prune"' _ "$doctor"
no "doctor reports no shell env warning" sh -c 'echo "$1" | grep -q "warn: shell"' _ "$doctor"

finish
