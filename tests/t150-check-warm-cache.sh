#!/bin/bash
# ci: macos
# A check starts warm (docs/checks.md): its copy of the state gets the cache of the state by the
# rules of `line new <state>`, relocated to the copy, the check can write there, and the line's own cache stays as
# it was.
. "$(dirname "$0")/lib.sh"
needs_nested_sandbox
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
printf '[package]\nname = "p"\nversion = "0.1.0"\n' > "$T/src/Cargo.toml"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
# A cargo unit that names this line by a compiled-in path (env!("CARGO_MANIFEST_DIR")).
mkdir -p target/debug/deps target/debug/.fingerprint/p-aaa; echo warm > target/built
printf '%s/target/debug/deps/p-aaa.d: src/lib.rs\n\nsrc/lib.rs:\n\n# env-dep:CARGO_MANIFEST_DIR=%s\n' "$PWD" "$PWD" > target/debug/deps/p-aaa.d
# A registry crate built from a CARGO_HOME that this machine does not have.
printf '%s/target/debug/deps/serde-eee.d: /nowhere/.cargo/registry/src/idx/serde/lib.rs\n' "$PWD" > target/debug/deps/serde-eee.d
l save --glasses -q
# The checked head has no cache of its own: it takes the cache of its parent, as `line new` does.
echo change > change.txt; l add change.txt >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
l config check.unit 'test "$(cat target/built)" = warm && test ! -e target/debug/.fingerprint/p-aaa && echo more > target/new && touch "target/by-$LAYR_TRY_STATE"' >/dev/null
l config checks.lines '{"main":["unit"]}' >/dev/null
l config --local checks.runner true >/dev/null
out=$(l checks watch --once --json 2>"$T/watch.err")
no "the check finds the cache of its state and writes in it" grep -q '"exit":1' <<<"$out"
has "it ran on both heads of main" '"count":2' "$out"
has "the copy's unit that named the line builds again" "1 units named the old path" "$(cat "$T/watch.err")"
has "a registry that this machine does not have is named" "registry at /nowhere/.cargo/registry/src, which this machine does not have" "$(cat "$T/watch.err")"
no "the line's own cache does not get the check's file" test -e target/new
eq "the line's cache is the same" warm "$(cat target/built)"
ok "the copy's unit that named the line built again there, the line keeps its own" test -d target/debug/.fingerprint/p-aaa
# The check of a protected head gives its caches back, saved with that head, on the line main.checks-<platform>.
plat=$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/macos/')-$(uname -m | sed 's/arm64/aarch64/')
has "the check gives its caches back on its own line" "main.checks-$plat" "$(l line list 2>&1)"
head=$(l rev-parse main)
l line new w "$head" >/dev/null 2>&1
eq "a new line from the checked head takes the check's caches" more "$(cat "$LAYR_ROOT/projects/p/lines/w/target/new" 2>&1)"
has "the old cache states of that line go at once" "0 unreachable state(s)" "$(l fsck --glasses 2>&1)"
no "the check's state is not in the line" grep -qx "target/built" <<<"$(l ls-files)"
has "the check copy leaves no overlay line behind" "0 overlay line(s) of deleted lines" "$(l fsck --glasses 2>&1)"

# `checks run --state`: the jobs of one exact state, such as a Git commit that CI fetched.
l config checks.changes '["unit"]' >/dev/null
l line new pr main >/dev/null 2>&1
cd "$LAYR_ROOT/projects/p/lines/pr" || exit 1
echo pr > pr.txt; l add pr.txt >/dev/null; l save -qm pr
s=$(l rev-parse pr)
out=$(l checks run --state "$s" --json 2>&1)
if [ "$(uname -s)" = Darwin ]; then
  # On macOS a state off main runs only with a maintainer's approval of that state.
  has "a state without approval stays pending" '"count":0,"pending":1' "$out"
  l approve -q "$s" >/dev/null
  out=$(l checks run --state "$s" --json 2>&1)
fi
has "run executes the change check of that state" '"count":1' "$out"
has "with its cache" '"exit":0' "$out"
has "a second run finds nothing to do" '"count":0,"pending":0' "$(l checks run --state "$s" --json 2>&1)"
l line new w3 "$s" >/dev/null 2>&1
# The pr line started warm from main's check cache: only the marker of the pr check itself must be missing.
by() { (cd "$LAYR_ROOT/projects/p/lines/$1/target" && ls -1 | grep '^by-' | sort); }
eq "a state off the protected lines gives no caches back" "$(by pr)" "$(by w3)"
no "and gets no checks line" grep -q "pr.checks-" <<<"$(l line list 2>&1)"
eq "pending agrees" 0 "$(l checks pending --state "$s" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')"
no "run needs --state" l checks run
finish
