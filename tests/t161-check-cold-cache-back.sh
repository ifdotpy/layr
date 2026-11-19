#!/bin/bash
# ci: macos
# Cross-location reuse in this fixture is explicitly opted in with cache-portable/cache.portable.
# A check that starts cold gives its cache back too (docs/checks.md): no cache warmed the copy, so the check made its
# cache folder as a plain folder; the save-back makes it a cache first, so the first build of a platform is saved.
# A pending fast-forward queue entry into main gives its caches back as main's next head: a server queue merges it
# only after its checks passed, so none runs on the head.
# A queue candidate (main's head merged with a pending entry) gives its caches back the same way.
. "$(dirname "$0")/lib.sh"
needs_nested_sandbox
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable cache-portable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
c=$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json)
c_id=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
c_key=$(echo "$c" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote trust archive "$c_id" --public-key "$c_key" >/dev/null
dl() { env LAYR_ROOT="$T/d/store" "$LAYR" "$@"; }
d=$(dl remote self --json)
l remote trust archive "$(echo "$d" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')" \
    --public-key "$(echo "$d" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')" >/dev/null

echo main > stamp; l add stamp >/dev/null; l save -qm change
l protect main >/dev/null 2>&1 || true
l config check.unit 'mkdir -p target && cat stamp > target/a' >/dev/null
l config checks.lines '{"main":["unit"]}' >/dev/null
l config checks.changes '["unit"]' >/dev/null
l push archive -q >/dev/null 2>&1
head=$(l rev-parse main)

cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
cl clone "file://$T/archive" p >/dev/null 2>&1
cd "$T/c/store/projects/p/lines/main" || exit 1
cl config --local checks.runner true >/dev/null
out=$(cl checks run --state main --json 2>/dev/null)
has "the runner checks main's head" '"exit":0' "$out"
cl push origin -q >/dev/null 2>&1

cd "$P/main" || exit 1
l fetch archive -q >/dev/null 2>&1
eq "the laptop has the runner's result" 0 "$(l checks pending --state main --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')"
l line new w "$head" >/dev/null 2>&1
eq "a new line of the laptop from that head takes the cache that the check built" main "$(cat "$P/w/target/a" 2>&1)"

for kind in fast-forward titled; do
  cd "$P/main" || exit 1
  l line new "$kind" >/dev/null 2>&1
  cd "$P/$kind" || exit 1
  echo "$kind" > stamp; l save -aqm "$kind" >/dev/null
  entry=$(l rev-parse "$kind")
  # macOS runs a change's checks unsandboxed: only an approved one.
  l approve "$entry" >/dev/null 2>&1
  if [ "$kind" = titled ]; then l queue add "$kind" -m "feat: titled" >/dev/null; else l queue add "$kind" >/dev/null; fi
  l push archive -q >/dev/null 2>&1
  cd "$T/c/store/projects/p/lines/main" || exit 1
  cl fetch origin -q >/dev/null 2>&1
  out=$(cl checks run --state "$entry" --json 2>/dev/null)
  has "the runner checks the $kind entry" '"exit":0' "$out"
  cl push origin -q >/dev/null 2>&1
  cd "$P/main" || exit 1
  l fetch archive -q >/dev/null 2>&1
  l line new "w-$kind" "$entry" >/dev/null 2>&1
  l queue remove "$(l queue list --json | python3 -c 'import json,sys; print([e["id"] for e in json.load(sys.stdin) if e["status"]=="pending"][0])')" >/dev/null 2>&1
done
eq "a fast-forward entry's check gives its caches back as main's next head" fast-forward "$(cat "$P/w-fast-forward/target/a" 2>&1)"
no "a titled entry merges as a new state: its check gives nothing back" grep -qx titled "$P/w-titled/target/a"

# A queue candidate gives its caches back as main's next head too: the merge job checks main's head merged with
# a pending entry that is no fast-forward, and moves main to that merge after its checks passed.
cd "$P/main" || exit 1
base=$(l log --json -n 1 "$head" | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["parents"][0])')
l config check.unit 'mkdir -p target && cat stamp $(ls extra 2>/dev/null) > target/a' >/dev/null
l line new cand-entry "$base" >/dev/null 2>&1
cd "$P/cand-entry" || exit 1
echo candidate > extra; l add extra >/dev/null; l save -qm "add extra" >/dev/null
entry=$(l rev-parse cand-entry)
l queue add cand-entry >/dev/null
l line new cand "$head" >/dev/null 2>&1
cd "$P/cand" || exit 1
l merge "$entry" >/dev/null 2>&1
candidate=$(l rev-parse cand)
id() { l log --json -n 1 "$1" | python3 -c 'import json,sys; r=json.loads(sys.stdin.readline()); print(" ".join(r[sys.argv[1]]) if sys.argv[1] == "parents" else r["id"])' "${2:-id}"; }
eq "the candidate merges main's head with the entry" "$(id "$head") $(id "$entry")" "$(id "$candidate" parents)"
l approve "$candidate" >/dev/null 2>&1
l push archive -q >/dev/null 2>&1
cd "$T/c/store/projects/p/lines/main" || exit 1
cl fetch origin -q >/dev/null 2>&1
out=$(cl checks run --state "$candidate" --json 2>/dev/null)
has "the runner checks the candidate" '"exit":0' "$out"
cl push origin -q >/dev/null 2>&1
cd "$P/main" || exit 1
l fetch archive -q >/dev/null 2>&1
l line new w-cand "$candidate" >/dev/null 2>&1
eq "a queue candidate's check gives its caches back as main's next head" "$(printf 'main\ncandidate')" "$(cat "$P/w-cand/target/a" 2>&1)"
l queue remove "$(l queue list --json | python3 -c 'import json,sys; print([e["id"] for e in json.load(sys.stdin) if e["status"]=="pending"][0])')" >/dev/null 2>&1
l config check.unit 'mkdir -p target && cat stamp > target/a' >/dev/null

# The runner's queue (a server queue) moves its own main to the entry. A machine whose main stays an older head
# still fetches the cache saved with the runner's head: --line follows every machine's head of main.
cd "$T" || exit 1
dl clone "file://$T/archive" p --heads --line main >/dev/null 2>&1
cd "$P/main" || exit 1
entry=$(l rev-parse fast-forward)
l queue add fast-forward >/dev/null
l push archive -q >/dev/null 2>&1
cd "$T/c/store/projects/p/lines/main" || exit 1
cl fetch origin -q >/dev/null 2>&1
cl queue run --into main >/dev/null 2>&1
eq "the runner's main moved to the entry" "$entry" "$(cl --line main rev-parse main 2>&1)"
cl push origin -q >/dev/null 2>&1
cd "$T/d/store/projects/p/lines/main" || exit 1
dl fetch origin -q --heads --line main >/dev/null 2>&1
no "the other machine's own main is still the older head" test "$(dl --line main rev-parse main 2>&1)" = "$entry"
dl line new from-runner "$entry" >/dev/null 2>&1
eq "--line main takes the cache of another machine's head of main" fast-forward "$(cat "$T/d/store/projects/p/lines/from-runner/target/a" 2>&1)"
finish
