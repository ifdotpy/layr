#!/bin/bash
# ci: macos overlayfs
# A cache state on a store remote that names a chunk no pack holds is not restored, and the check runs before any
# file is read. A fetch remembers the failure: the next fetch does not try the state again while the packs of the
# remote stay the same, and tries once more after a push changed them. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target/debug/deps
python3 -c 'import os,sys; open(sys.argv[1],"wb").write(os.urandom(3 << 20))' target/debug/deps/libbig.rlib
l save --glasses -q
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
ok "push the cache to the store" l push archive -q

mkdir -p "$T/b"
B() { env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" "$@"; }
B clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/b/store/projects/p/lines/main
ok "the clone has the project" test -d "$Q"
cd "$P/main" || exit 1
python3 -c 'import os,sys; open(sys.argv[1],"wb").write(os.urandom(1 << 20))' target/debug/deps/libmore.rlib
l save --glasses -q
ok "push a newer cache" l push archive -q
# The packs of the cache go, as a broken sweep or a stopped push would leave them; the tree still names the chunks.
find "$T/archive" -path '*/chunks/*' -name '*.age' -exec rm -f {} +
cd "$Q" || exit 1
out=$(LAYR_TRACE=1 B fetch origin 2>&1)
has "the fetch omits the cache state with the missing chunk" "is not in the store" "$out"
eq "no file of it was read" "" "$(echo "$out" | grep 'store restore files' | grep -v ' 0\.[0-9] ms')"
out=$(B fetch origin 2>&1)
has "the next fetch does not try it again" "it failed before, and the remote did not change" "$out"
cd "$P/main" || exit 1
echo again > target/debug/again
l save --glasses -q
l push archive -q
cd "$Q" || exit 1
out=$(B fetch origin 2>&1)
no "after a push changed the packs it is tried again" grep -q "it failed before" <<<"$out"
finish
