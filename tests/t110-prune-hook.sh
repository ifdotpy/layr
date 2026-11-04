#!/bin/bash
# The cache adapter hook (issue #167, docs/cache-overlays.md section 18). The hook prunes the cache folders
# that `line new`, `merge --caches-only`, a merge, a follow pass and `save --caches` take, to the live set of the
# cargo adapter, before the next step. `prune=none` keeps a folder. An unknown adapter name is logged and does
# not fail the command. One user, so it runs on macOS and on Linux.
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

# 1. Stale units in the saved cache of main: prune is off while they are saved.
ok "the owner sets prune=none on the cache" l overlay set cache.target --prune none
cargo_quiet build --workspace --features extra -q >/dev/null
stale=$(units target)
[ "$stale" -gt "$base" ] && { PASS=$((PASS + 1)); echo "PASS the feature build made stale units ($base to $stale)"; } || { FAIL=$((FAIL + 1)); echo "FAIL the feature build made stale units ($base to $stale)"; }
l save --caches -q
eq "prune=none keeps the stale units in the working copy" "$stale" "$(units target)"
ok "the owner sets prune to auto" l overlay set cache.target --prune auto

# 2. `line new` prunes the cache it takes.
l line new fresh main >/dev/null
eq "line new removes the stale units" "$base" "$(units $P/fresh/target)"
rebuilt=$(cd "$P/fresh" && cargo_quiet build --workspace)
case "$rebuilt" in *Compiling*) FAIL=$((FAIL + 1)); echo "FAIL the next build of the new line compiled" ;; *) PASS=$((PASS + 1)); echo "PASS the next build of the new line compiles nothing" ;; esac

# 3. `merge --caches-only` prunes the cache it takes from main.
cd "$P/other"
l merge main --caches-only -q >/dev/null 2>&1
eq "merge --caches-only removes the stale units" "$base" "$(units $P/other/target)"

# 4. prune=none keeps the folder: a new line from the stale cache keeps its stale units.
cd "$P/main"
ok "the owner sets prune=none again" l overlay set cache.target --prune none
has "overlay list shows prune=none" "prune=none" "$(l overlay list)"
l line new keep main >/dev/null 2>&1
eq "prune=none keeps the stale units" "$stale" "$(units $P/keep/target)"

# 5. A plain `save --caches` prunes the folder before it saves: the saved cache has no stale units.
ok "the owner sets prune to auto" l overlay set cache.target --prune auto
cargo_quiet build --workspace --features extra -q >/dev/null
eq "the working copy has stale units before the save" "$stale" "$(units target)"
l save --caches -q
eq "save --caches prunes the working copy first" "$base" "$(units target)"
l line new after main >/dev/null 2>&1
eq "the cache that the save made has no stale units" "$base" "$(units $P/after/target)"

# 6. An unknown adapter name does not fail the command; the log names it.
ok "the owner sets an adapter name that does not exist" l overlay set cache.target --prune nosuch
out=$(l line new bad main 2>&1); rc=$?
eq "line new succeeds with an unknown adapter" "0" "$rc"
has "the log names the unknown adapter" "unknown prune adapter 'nosuch'" "$out"
eq "the line is there" "1" "$(l line list | grep -c '^ *bad$' | tr -d ' ')"

finish
