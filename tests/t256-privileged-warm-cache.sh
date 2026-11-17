#!/bin/bash
# ci: overlayfs
# A privileged check starts warm too (docs/checks.md): its root copy gets the cache folders of the check copy.
# A Btrfs snapshot leaves out nested subvolumes, and each cache of the check copy is one, so a root copy made only
# from a snapshot had an empty target/ and every privileged suite built from cold.
. "$(dirname "$0")/lib.sh"
if [ "$(uname -s)" != Linux ] || [ "$(id -u)" != 0 ]; then
    echo "$(basename "$0") (not as root on Linux): 0 passed, 0 failed"; exit 0
fi
id layr-check-reader >/dev/null 2>&1 || useradd -m layr-check-reader
sample "$T/src"
echo 'target regenerable cache-portable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l access grant layr-check-reader reader >/dev/null
mkdir -p target/debug/deps; echo warm > target/built; echo unit > target/debug/deps/p-aaa.rlib
l save --overlays -q
l protect main >/dev/null 2>&1 || true
l config checks.lines '{"main":["root"]}' >/dev/null
l config checks.privileged '["root"]' >/dev/null
l config check.root 'test "$(id -u)" = 0 && test "$(cat target/built)" = warm && test -s target/debug/deps/p-aaa.rlib && echo more > target/new' >/dev/null
l config --local checks.runner true >/dev/null
l config --local checks.privileged-runner true >/dev/null
chmod 755 "$T"
cp "$LAYR" "$T/layr"
export LAYR_SOCKET=$T/layr.sock
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null || true; cleanup' EXIT
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep .1; done
head=$(l rev-parse main)
out=$(cd / && runuser -u layr-check-reader -- env LAYR_SOCKET="$LAYR_SOCKET" "$T/layr" --project p \
    checks run --state "$head" --privileged-only --json 2>"$T/run.err")
[ ! -s "$T/run.err" ] || cat "$T/run.err"
has "the privileged check of main's head ran" '"count":1' "$out"
has "its root copy has the cache of the state" '"exit":0' "$out"
eq "the line's own cache is the same" warm "$(cat target/built)"
no "and does not get the check's file" test -e target/new

# Location-bound output (`cache-location-bound`) from the line must stay cold. A sandboxed check then builds at
# /work and saves back there; the privileged root copy must reuse that compatible location.
echo 'target regenerable cache-location-bound' > "$T/src/.layrattributes"
l init bound --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/bound/lines/main" || exit 1
l access grant layr-check-reader reader >/dev/null
echo foreign > target/foreign
l save --overlays -q
l protect main >/dev/null 2>&1 || true
l config check.cold 'test "$(id -u)" = 0 && test ! -e target/foreign && test ! -e target/built && grep -q /work/target target/.layr-cache-location' >/dev/null
l config check.seed 'test ! -e target/foreign && test ! -e target/built && mkdir -p target/debug/deps && echo warm > target/built && echo unit > target/debug/deps/p-aaa.rlib' >/dev/null
l config check.root 'test "$(id -u)" = 0 && test "$(cat target/built)" = warm && test -s target/debug/deps/p-aaa.rlib && grep -q /work/target target/.layr-cache-location && echo more > target/new' >/dev/null
l config checks.lines '{"main":["cold","seed","root"]}' >/dev/null
l config checks.privileged '["cold","root"]' >/dev/null
l config checks.after '{"seed":["cold"],"root":["seed"]}' >/dev/null
l config --local checks.runner true >/dev/null
l config --local checks.privileged-runner true >/dev/null
head=$(l rev-parse main)
for check in cold seed root; do
    out=$(cd / && runuser -u layr-check-reader -- env LAYR_SOCKET="$LAYR_SOCKET" "$T/layr" --project bound \
        checks run --state "$head" --only "$check" --json 2>"$T/bound.err")
    [ ! -s "$T/bound.err" ] || cat "$T/bound.err"
    has "the location-bound $check check ran" '"count":1' "$out"
    has "the location-bound $check check passed" '"exit":0' "$out"
done
eq "the bound line's cache stays intact" foreign "$(cat target/foreign)"
no "the bound line does not get the check's build" test -e target/built
no "the bound line does not get the root check's file" test -e target/new
finish
