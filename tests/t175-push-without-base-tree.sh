#!/bin/bash
# ci: macos overlayfs
# A cache state whose parent's tree is gone from the remote (a sweep deleted it before this machine knew of a state
# built on it, or another machine's sweep did) goes up as a complete tree. Before, the push failed on the missing
# object, or made a delta on a base that nobody can read.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
read c_id c_key <<<"$(env LAYR_ROOT="$T/c/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$c_id" --public-key "$c_key" >/dev/null
mkdir -p target && head -c 20000 /dev/urandom > target/a && l save --glasses -q
l push cloud -q >/dev/null 2>&1
# The tree of the cache state that the next one is built on goes from the remote.
gone=$(ls -t $(find "$T/archive/overlays" -name '*.tree.age') | head -1)
rm "$gone"
# This machine's tree cache does not hold it either (the case of a state that another machine built on it).
find "$LAYR_ROOT/projects/p/meta" -name 'push-cache.sqlite3*' -delete
echo b > target/b && l save --glasses -q
out=$(l push cloud 2>&1)
ok "the push of a state whose base tree is gone succeeds" test $? = 0
no "and names no missing object" grep -q "missing remote object" <<<"$out"

cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
(cd "$T" && cl clone "file://$T/archive" p --line main >/dev/null 2>&1)
(cd "$T/c/store/projects/p/lines/main" && cl line new w main >/dev/null 2>&1)
eq "another machine reads the new cache" b "$(cat "$T/c/store/projects/p/lines/w/target/b" 2>&1)"
finish
