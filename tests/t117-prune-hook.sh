#!/bin/bash
# ci: macos overlayfs
# The cache adapter hook (issue #167, docs/cache-overlays.md section 19). The hook prunes the cache folders
# that `line new`, `merge --glasses-only`, a merge, a follow pass and `save --glasses` take, to the live set of the
# cargo adapter, before the next step. `prune=none` keeps a folder. An unknown adapter name is logged and does
# not fail the command.
#
# The hook runs as the line's owner, and root is never pruned. On a non-root run (macOS, Linux as a user) the lines
# belong to the user. On a root run (the Linux CI job) the lines belong to a test user, so the checks run as that
# user, and the last check is the refusal of a root-owned line.
. "$(dirname "$0")/lib.sh"
skip_in_check_sandbox "its cache worker runs cargo with the OS home"
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
# PENV: the project env of the metadata below (`[workspace.metadata.layr-retest] env`). Every cargo call of this
# test uses it, so the standard units are the ones the sweep keeps. A build with another env makes other units.
PENV="CARGO_INCREMENTAL=1 RUSTC_WRAPPER="
# cb <folder> <cargo args...>: cargo in a line folder, as the owner of the line.
cb() { local dir=$1; shift; (cd "$dir" && owned env CARGO_NET_OFFLINE=true $PENV cargo "$@" 2>&1); }

# A cargo project. The standard build is `build --workspace`; `--features extra` makes stale units.
cd /tmp || exit 1
SRC=$T/src
rm -rf "$SRC"; mkdir -p "$SRC/src"
printf '[package]\nname = "prunehook"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nextra = []\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [["build", "--workspace"]]\nenv = { CARGO_INCREMENTAL = "1", RUSTC_WRAPPER = "" }\n' > "$SRC/Cargo.toml"
echo 'pub fn a() -> i32 {' > "$SRC/src/lib.rs"; echo '    1' >> "$SRC/src/lib.rs"; echo '}' >> "$SRC/src/lib.rs"
echo 'target regenerable' > "$SRC/.layrattributes"
printf '/target\n' > "$SRC/.gitignore"
l init p --from "$SRC" $OWNER_ARG -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
units() { ls "$1/debug/.fingerprint" 2>/dev/null | wc -l | tr -d ' '; }
# times_mismatch <source target> <copy target>: the files of deps and .fingerprint that both have, whose
# modification times differ (a count, and the first names).
times_mismatch() {
    python3 - "$1" "$2" <<'PYTIMES'
import os, sys
src, dst = sys.argv[1], sys.argv[2]
bad = []
for sub in ("debug/deps", "debug/.fingerprint"):
    for root, _, files in os.walk(os.path.join(src, sub)):
        for n in files:
            s = os.path.join(root, n)
            d = os.path.join(dst, os.path.relpath(s, src))
            if os.path.islink(s) or not os.path.isfile(d):
                continue
            if os.stat(s).st_mtime_ns != os.stat(d).st_mtime_ns:
                bad.append(os.path.relpath(s, src))
print(len(bad))
for b in bad[:3]:
    print("differs:", b, file=sys.stderr)
    print("  working copy of the source:", os.stat(os.path.join(src, b)).st_mtime_ns, file=sys.stderr)
    print("  copy in the line:          ", os.stat(os.path.join(dst, b)).st_mtime_ns, file=sys.stderr)
    states = os.environ.get("LAYR_CACHE_STATES", "")
    for st in sorted(os.listdir(states)) if states and os.path.isdir(states) else []:
        f = os.path.join(states, st, b)
        if os.path.isfile(f):
            print("  cache state", st[:12], os.stat(f).st_mtime_ns, file=sys.stderr)
PYTIMES
}
new_line() { l line new $OWNER_ARG "$@" >/dev/null; }

cd "$P/main"
# A cache that root saves belongs to root: another user takes it only from a protected line (its caches are shared).
# So on a root run main stays protected, and on a user run the protection is removed for direct saves.
if [ "$(id -u)" != 0 ]; then l protect main --remove >/dev/null 2>&1 || true; fi
cb "$P/main" build --workspace -q >/dev/null
base=$(units $P/main/target)
[ "$base" -gt 0 ] && { PASS=$((PASS + 1)); echo "PASS the standard build made units ($base)"; } || { FAIL=$((FAIL + 1)); echo "FAIL the standard build made no units: the owner cannot build"; cb "$P/main" build --workspace 2>&1 | tail -20; }
(cd "$P/main" && l save --glasses -q)
# The times are compared before any cargo runs in a line: a cargo run rewrites the files it compiles.
export LAYR_CACHE_STATES=$LAYR_ROOT/projects/p/meta/overlays/$TG/states

# The union copy alone, with the prune off: the copied times are the source's (cargo compares them).
new_line other2 main
ok "the owner sets prune=none for the union check" l overlay set $TG --prune none
(cd "$P/other2" && l merge main --glasses-only -q >/dev/null 2>&1)
eq "the union copy keeps the times without a prune" "0" "$(times_mismatch $P/main/target $P/other2/target)"
ok "the owner sets prune to auto after the union check" l overlay set $TG --prune auto
new_line other main

# 1. Stale units in the saved cache of main: prune is off while they are saved.
ok "the owner sets prune=none on the cache" l overlay set $TG --prune none
cb "$P/main" build --workspace --features extra -q >/dev/null
stale=$(units $P/main/target)
[ "$stale" -gt "$base" ] && { PASS=$((PASS + 1)); echo "PASS the feature build made stale units ($base to $stale)"; } || { FAIL=$((FAIL + 1)); echo "FAIL the feature build made stale units ($base to $stale)"; }
(cd "$P/main" && l save --glasses -q)
eq "prune=none keeps the stale units in the working copy" "$stale" "$(units $P/main/target)"
ok "the owner sets prune to auto" l overlay set $TG --prune auto

# 2. `line new` takes the cache as it is: no adapter runs, so the stale units come along. The next save prunes them.
out=$(l line new $OWNER_ARG fresh main 2>&1)
no "line new prints no prune line" sh -c "echo \"\$1\" | grep -q 'layr: prune'" _ "$out"
eq "line new takes the stale units (no prune at take)" "$stale" "$(units $P/fresh/target)"
(cd "$P/fresh" && l save --glasses -q)
eq "save --glasses of the new line prunes its cache" "$base" "$(units $P/fresh/target)"
rebuilt=$(cb "$P/fresh" build --workspace)
case "$rebuilt" in *Compiling*) FAIL=$((FAIL + 1)); echo "FAIL the next build of the new line compiled" ;; *) PASS=$((PASS + 1)); echo "PASS the next build of the new line compiles nothing" ;; esac

# 3. `merge --glasses-only` takes the units of main into `other` (union), with no prune. The next save of `other`
# prunes the union to the live set: the saved cache has only the units of the standard build.
out=$(cd "$P/other" && l merge main --glasses-only 2>&1)
no "merge --glasses-only prints no prune line" sh -c "echo \"\$1\" | grep -q 'layr: prune'" _ "$out"
merged=$(units $P/other/target)
mismatch=$(times_mismatch $P/main/target $P/other/target)
eq "merge --glasses-only keeps the units of both sides until the next save" "$stale" "$merged"
eq "the union merge keeps the times of the cache" "0" "$mismatch"
(cd "$P/other" && l save --glasses -q)
eq "save --glasses after the union merge keeps only the live units" "$base" "$(units $P/other/target)"
rebuilt=$(cb "$P/other" build --workspace)
case "$rebuilt" in *Compiling*) FAIL=$((FAIL + 1)); echo "FAIL the next build after the union merge compiled" ;; *) PASS=$((PASS + 1)); echo "PASS the next build after the union merge compiles nothing" ;; esac

# 4. prune=none keeps the folder: a new line from the stale cache keeps its stale units.
ok "the owner sets prune=none again" l overlay set $TG --prune none
has "overlay list shows prune=none" "prune=none" "$(l overlay list)"
new_line keep main
eq "prune=none keeps the stale units" "$stale" "$(units $P/keep/target)"
if [ "$(id -u)" = 0 ]; then
    # Root: a root-owned line is never pruned, by save either.
    rootout=$(l line new rootline main 2>&1)
    rootsave=$(cd "$P/rootline" && l save --glasses -q 2>&1)
    eq "a root-owned line keeps the stale units (root is never pruned)" "$stale" "$(units $P/rootline/target)"
    no "a root-owned line prints no prune line" sh -c "echo \"\$1\" | grep -q 'layr: prune'" _ "$rootsave$rootout"
fi

# 5. A plain `save --glasses` prunes the folder before it saves: the saved cache has no stale units.
ok "the owner sets prune to auto" l overlay set $TG --prune auto
cb "$P/main" build --workspace --features extra -q >/dev/null
eq "the working copy has stale units before the save" "$stale" "$(units $P/main/target)"
(cd "$P/main" && l save --glasses -q)
eq "save --glasses prunes the working copy first" "$base" "$(units $P/main/target)"
new_line after main
eq "the cache that the save made has no stale units" "$base" "$(units $P/after/target)"

# 6. An unknown adapter name does not fail the command; the log names it.
ok "the owner sets an adapter name that does not exist" l overlay set $TG --prune nosuch
out=$(l line new $OWNER_ARG bad main 2>&1); rc=$?
eq "line new succeeds with an unknown adapter" "0" "$rc"
eq "the line is there" "1" "$(l line list | grep -c '^ *bad$' | tr -d ' ')"
# The hook runs at the save: the log names the unknown adapter, and the save still succeeds.
out=$(cd "$P/bad" && l save --glasses 2>&1); rc=$?
eq "save --glasses succeeds with an unknown adapter" "0" "$rc"
has "the log names the unknown adapter" "unknown prune adapter 'nosuch'" "$out"
ok "the owner sets prune to auto again" l overlay set $TG --prune auto

# 7. A cargo that the owner cannot run: the adapter names the reason, and the folder is not pruned. (retest finds
#    a rustup install outside PATH, so the check puts a cargo that fails first on PATH.)
ok "the owner sets prune to auto for the cargo-missing check" l overlay set $TG --prune auto
mkdir -p "$T/badcargo" && printf '#!/bin/sh\necho "cargo: not installed" >&2\nexit 127\n' > "$T/badcargo/cargo" && chmod 755 "$T/badcargo" "$T/badcargo/cargo"
(cd "$P/other" && owned env PATH="$T/badcargo:/usr/bin:/bin" "$LAYR" retest prune target) > "$T/nocargo" 2>&1 || true
has "a missing cargo prints the reason" "skipped: cargo metadata: cargo failed: cargo: not installed" "$(cat "$T/nocargo")"
eq "a missing cargo keeps the units" "$base" "$(units $P/other/target)"

finish
