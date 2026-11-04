#!/bin/bash
# ci: macos overlayfs
# The cache adapter hook (issue #167, docs/cache-overlays.md section 19). The hook prunes the cache folders
# that `line new`, `merge --caches-only`, a merge, a follow pass and `save --caches` take, to the live set of the
# cargo adapter, before the next step. `prune=none` keeps a folder. An unknown adapter name is logged and does
# not fail the command.
#
# The hook runs as the line's owner, and root is never pruned. On a non-root run (macOS, Linux as a user) the lines
# belong to the user. On a root run (the Linux CI job) the lines belong to a test user, so the checks run as that
# user, and the last check is the refusal of a root-owned line.
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
    owned() { "$@"; }
    OWNER_ARG=""
fi
# cb <folder> <cargo args...>: cargo in a line folder, as the owner of the line.
cb() { local dir=$1; shift; (cd "$dir" && owned env CARGO_NET_OFFLINE=true RUSTC_WRAPPER= CARGO_INCREMENTAL=1 cargo "$@" 2>&1); }

# A cargo project. The standard build is `build --workspace`; `--features extra` makes stale units.
cd /tmp || exit 1
SRC=$T/src
rm -rf "$SRC"; mkdir -p "$SRC/src"
printf '[package]\nname = "prunehook"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nextra = []\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [["build", "--workspace"]]\n' > "$SRC/Cargo.toml"
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
(cd "$P/main" && l save --caches -q)
new_line other main
# The union copy alone, with the prune off: if the times are right here, a prune of the merged cache rewrote them.
new_line other2 main
ok "the owner sets prune=none for the union check" l overlay set cache.target --prune none
(cd "$P/other2" && l merge main --caches-only -q >/dev/null 2>&1)
eq "the union copy keeps the times without a prune" "0" "$(times_mismatch $P/main/target $P/other2/target)"
# Which entries the sweep removes from a merged cache (a diagnostic: the names removed by the prune command).
new_line other3 main
ok "the owner sets prune=none for the sweep check" l overlay set cache.target --prune none
(cd "$P/other3" && l merge main --caches-only -q >/dev/null 2>&1)
ok "the owner sets prune to auto after the sweep check" l overlay set cache.target --prune auto
(cd "$P/other3/target" && find . | sort) > "$T/names-before3"
# Cargo's own reason for the merged unit, before the prune runs: the dirty, fresh and compiling lines of a plain -v build.
(cd "$P/other3" && owned env CARGO_NET_OFFLINE=true RUSTC_WRAPPER= CARGO_BUILD_JOBS=1 cargo build --workspace -v 2>&1 | grep -m 6 -E "Dirty|Fresh|Compiling|dirty|stale" | cut -c1-400) | sed 's/^/DIAG reason: /' || true
# The artifact names of the library in the listing that the prune uses (json), as the sweep reads them.
(cd "$P/other3" && owned env CARGO_NET_OFFLINE=true RUSTC_WRAPPER= CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=target cargo build --workspace -v --message-format json 2>/dev/null | python3 -c "
import sys, json
for line in sys.stdin:
    try: v = json.loads(line)
    except Exception: continue
    if v.get('reason') == 'compiler-artifact' and v['target']['kind'] == ['lib']:
        print('DIAG listing artifacts: fresh=%s %s' % (v.get('fresh'), [f.split('/')[-1] for f in v.get('filenames', [])]))
") || true
for d in main other3; do python3 -c "import os,sys; print('DIAG mtime', sys.argv[1], os.stat(sys.argv[2]).st_mtime_ns, sys.argv[2])" $d "$P/$d/src/lib.rs"; done
(cd "$P/other3" && owned env CARGO_NET_OFFLINE=true "$LAYR" retest prune target) > "$T/prune-out3" 2>&1 || true
(cd "$P/other3/target" && find . | sort) > "$T/names-after3"
echo "DIAG prune output: $(cat "$T/prune-out3" | tr '\n' ' ')"
echo "DIAG removed by the sweep: $(comm -23 "$T/names-before3" "$T/names-after3" | head -20 | tr '\n' ' ')"
# Why cargo treats the merged standard unit as dirty (a diagnostic: the reason cargo prints for the first dirty unit).
echo "DIAG cargo on the merged line without prune: $(cb "$P/other2" build --workspace -v 2>&1 | grep -m2 -E 'Dirty|Fresh' | tr '\n' ' ')"
ok "the owner sets prune to auto after the union check" l overlay set cache.target --prune auto

# 1. Stale units in the saved cache of main: prune is off while they are saved.
ok "the owner sets prune=none on the cache" l overlay set cache.target --prune none
cb "$P/main" build --workspace --features extra -q >/dev/null
stale=$(units $P/main/target)
[ "$stale" -gt "$base" ] && { PASS=$((PASS + 1)); echo "PASS the feature build made stale units ($base to $stale)"; } || { FAIL=$((FAIL + 1)); echo "FAIL the feature build made stale units ($base to $stale)"; }
(cd "$P/main" && l save --caches -q)
eq "prune=none keeps the stale units in the working copy" "$stale" "$(units $P/main/target)"
ok "the owner sets prune to auto" l overlay set cache.target --prune auto

# 2. `line new` prunes the cache it takes.
new_line fresh main
eq "line new removes the stale units" "$base" "$(units $P/fresh/target)"
rebuilt=$(cb "$P/fresh" build --workspace)
case "$rebuilt" in *Compiling*) FAIL=$((FAIL + 1)); echo "FAIL the next build of the new line compiled" ;; *) PASS=$((PASS + 1)); echo "PASS the next build of the new line compiles nothing" ;; esac

# 3. `merge --caches-only` prunes the cache it takes from main.
(cd "$P/other" && l merge main --caches-only -q)
merged=$(units $P/other/target)
# The times are compared before any cargo runs in the line: a cargo run rewrites the files it compiles.
export LAYR_CACHE_STATES=$LAYR_ROOT/projects/p/meta/overlays/cache.target/states
mismatch=$(times_mismatch $P/main/target $P/other/target)
if [ "$merged" != "$base" ]; then
    # Diagnostics for a failure: who owns the entries that the merge copied, and what the owner's listing sees.
    echo "DIAG other target units $merged, expected $base"
    ls -lan "$P/other/target/debug/.fingerprint" 2>&1 | head -12
    (cd "$P/other" && owned env CARGO_NET_OFFLINE=true RUSTC_WRAPPER= cargo build --workspace -v 2>&1 | head -12) || true
fi
eq "merge --caches-only removes the stale units" "$base" "$merged"
# The union copies main's cache into the folder that already has one: the copied times are the source's
# (cargo compares them). The mismatch count is over the files that both caches have.
eq "the union merge keeps the times of the cache" "0" "$mismatch"
rebuilt=$(cb "$P/other" build --workspace)
case "$rebuilt" in *Compiling*) FAIL=$((FAIL + 1)); echo "FAIL the next build after the union merge compiled" ;; *) PASS=$((PASS + 1)); echo "PASS the next build after the union merge compiles nothing" ;; esac

# 4. prune=none keeps the folder: a new line from the stale cache keeps its stale units.
ok "the owner sets prune=none again" l overlay set cache.target --prune none
has "overlay list shows prune=none" "prune=none" "$(l overlay list)"
new_line keep main
eq "prune=none keeps the stale units" "$stale" "$(units $P/keep/target)"

# 5. A plain `save --caches` prunes the folder before it saves: the saved cache has no stale units.
ok "the owner sets prune to auto" l overlay set cache.target --prune auto
cb "$P/main" build --workspace --features extra -q >/dev/null
eq "the working copy has stale units before the save" "$stale" "$(units $P/main/target)"
(cd "$P/main" && l save --caches -q)
eq "save --caches prunes the working copy first" "$base" "$(units $P/main/target)"
new_line after main
eq "the cache that the save made has no stale units" "$base" "$(units $P/after/target)"

# 6. An unknown adapter name does not fail the command; the log names it.
ok "the owner sets an adapter name that does not exist" l overlay set cache.target --prune nosuch
out=$(l line new $OWNER_ARG bad main 2>&1); rc=$?
eq "line new succeeds with an unknown adapter" "0" "$rc"
has "the log names the unknown adapter" "unknown prune adapter 'nosuch'" "$out"
eq "the line is there" "1" "$(l line list | grep -c '^ *bad$' | tr -d ' ')"
ok "the owner sets prune to auto again" l overlay set cache.target --prune auto

# 7. Root (only on a root run): a root-owned line is refused. It keeps the stale units, and prints no prune line.
if [ "$(id -u)" = 0 ]; then
    l overlay set cache.target --prune none >/dev/null
    cb "$P/main" build --workspace --features extra -q >/dev/null
    (cd "$P/main" && l save --caches -q)
    rootout=$(l line new rootline main 2>&1)
    eq "a root-owned line keeps the stale units (root is never pruned)" "$(units $P/main/target)" "$(units $P/rootline/target)"
    no "a root-owned line prints no prune line" sh -c "echo \"\$1\" | grep -q 'layr: prune'" _ "$rootout"
fi

finish
