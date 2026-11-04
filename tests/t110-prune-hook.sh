#!/bin/bash
# The cache adapter hook (issue #167, docs/cache-overlays.md section 18): `line new`, `merge
# --caches-only` and a merge prune the cache folders they take to the live set of the cargo adapter.
# `prune=none` keeps a folder. An unknown adapter name is logged and does not fail the command.
# One user, so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

# A cargo project. The standard build is `build --workspace`; `--features extra` makes stale units.
SRC=$T/src
rm -rf "$SRC"; mkdir -p "$SRC/src"
printf '[package]\nname = "prunehook"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nextra = []\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [["build", "--workspace"]]\n' > "$SRC/Cargo.toml"
echo 'pub fn a() -> i32 {' > "$SRC/src/lib.rs"; echo '    1' >> "$SRC/src/lib.rs"; echo '}' >> "$SRC/src/lib.rs"
echo 'target regenerable' > "$SRC/.layrattributes"
printf '/target\n' > "$SRC/.gitignore"
l init p --from "$SRC" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cargo_quiet() { env CARGO_NET_OFFLINE=true RUSTC_WRAPPER= CARGO_INCREMENTAL=1 cargo "$@" 2>&1; }
units() { ls "$1/debug/.fingerprint" 2>/dev/null | wc -l | tr -d ' '; }

cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
cargo_quiet build --workspace -q >/dev/null
base=$(units target)
l save --caches -q
l line new other main >/dev/null

# 1. The stale units of a feature build are in the cache of main. `line new` takes the cache and prunes it.
cargo_quiet build --workspace --features extra -q >/dev/null
stale=$(units target)
has "the feature build adds units to the cache" "$base" "$base"
[ "$stale" -gt "$base" ] && PASS=$((PASS + 1)) && echo "PASS the feature build made stale units ($base to $stale)" || { FAIL=$((FAIL + 1)); echo "FAIL the feature build made stale units ($base to $stale)"; }
l save --caches -q
l line new fresh main >/dev/null
eq "line new removes the stale units" "$base" "$(units $P/fresh/target)"
rebuilt=$(cd "$P/fresh" && cargo_quiet build --workspace)
case "$rebuilt" in *Compiling*) FAIL=$((FAIL + 1)); echo "FAIL the next build of the new line compiled" ;; *) PASS=$((PASS + 1)); echo "PASS the next build of the new line compiles nothing" ;; esac

# 2. `merge --caches-only` takes the cache of main into `other` (made before the feature build) and prunes it.
cd "$P/other"
l merge main --caches-only -q >/dev/null 2>&1
eq "merge --caches-only removes the stale units" "$base" "$(units $P/other/target)"

# 3. prune=none keeps the folder: the next line gets the stale units again.
cd "$P/main"
ok "the owner sets prune=none on the cache" l overlay set cache.target --prune none
has "overlay list shows prune=none" "prune=none" "$(l overlay list)"
l line new keep main >/dev/null 2>&1
eq "prune=none keeps the stale units" "$stale" "$(units $P/keep/target)"

# 4. An unknown adapter name does not fail the command; the log names it.
ok "the owner sets an adapter name that does not exist" l overlay set cache.target --prune nosuch
out=$(l line new bad main 2>&1); rc=$?
eq "line new succeeds with an unknown adapter" "0" "$rc"
has "the log names the unknown adapter" "unknown prune adapter 'nosuch'" "$out"
eq "the line is there" "1" "$(l line list | grep -c '^ *bad$' | tr -d ' ')"

finish
