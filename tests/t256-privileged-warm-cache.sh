#!/bin/bash
# A privileged check starts warm too (docs/checks.md): its root copy gets the cache folders of the check copy.
# A Btrfs snapshot leaves out nested subvolumes, and each cache of the check copy is one, so a root copy made only
# from a snapshot had an empty target/ and every privileged suite built from cold.
. "$(dirname "$0")/lib.sh"
if [ "$(uname -s)" != Linux ] || [ "$(id -u)" != 0 ]; then
    echo "$(basename "$0") (not as root on Linux): 0 passed, 0 failed"; exit 0
fi
id layr-check-reader >/dev/null 2>&1 || useradd -m layr-check-reader
sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
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
has "the privileged check of main's head ran" '"count":1' "$out"
has "its root copy has the cache of the state" '"exit":0' "$out"
eq "the line's own cache is the same" warm "$(cat target/built)"
no "and does not get the check's file" test -e target/new
finish
