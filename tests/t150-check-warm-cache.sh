#!/bin/bash
# ci: macos
# A check starts warm (docs/checks.md): its copy of the state gets the cache of the state by the
# rules of `line new <state>`, the check can write there, and the line's own cache stays as it was.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
mkdir -p target; echo warm > target/built; l save --caches -q
# The checked head has no cache of its own: it takes the cache of its parent, as `line new` does.
echo change > change.txt; l add change.txt >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
l config check.unit 'test "$(cat target/built)" = warm && echo more > target/new' >/dev/null
l config checks.lines '{"main":["unit"]}' >/dev/null
l config --local checks.runner true >/dev/null
out=$(l checks watch --once --json 2>&1)
no "the check finds the cache of its state and writes in it" grep -q '"exit":1' <<<"$out"
has "it ran on both heads of main" '"count":2' "$out"
no "the line's own cache does not get the check's file" test -e target/new
eq "the line's cache is the same" warm "$(cat target/built)"
l line new w main >/dev/null 2>&1
no "a new line does not get the check's file either" test -e "$LAYR_ROOT/projects/p/lines/w/target/new"
no "the check's state is not in the line" grep -qx "target/built" <<<"$(l ls-files)"
has "the check copy leaves no overlay line behind" "0 overlay line(s) of deleted lines" "$(l fsck --caches 2>&1)"

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
eq "pending agrees" 0 "$(l checks pending --state "$s" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')"
no "run needs --state" l checks run
finish
