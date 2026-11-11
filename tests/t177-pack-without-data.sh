#!/bin/bash
# ci: macos overlayfs
# A chunk pack whose data is gone while its index stays (a delete that removed the data and not the index): the
# next push does not stop on it, the sweep deletes the index, and the chunks of a new cache go up again, so another
# machine reads it.
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
mkdir -p target && head -c 300000 /dev/urandom > target/a && l save --caches -q
l push cloud -q >/dev/null 2>&1
packs=$(find "$T/archive/overlays" -name '*.pack.age')
ok "the cache went up as a chunk pack" test -n "$packs"
# The data goes, the index stays.
rm $packs
cp target/a target/b && l save --caches -q
out=$(l push cloud 2>&1)
ok "the next push does not stop on the pack without data" test $? = 0
no "it names no fatal error" grep -q "fatal" <<<"$out"
eq "the sweep deleted the index without data" "" "$(for p in $packs; do [ -e "${p%.pack.age}.index.age" ] && echo left; done)"
ok "the chunks went up again in a new pack" test -n "$(find "$T/archive/overlays" -name '*.pack.age')"

cl() { env LAYR_ROOT="$T/c/store" "$LAYR" "$@"; }
(cd "$T" && cl clone "file://$T/archive" p --line main >/dev/null 2>&1)
(cd "$T/c/store/projects/p/lines/main" && cl line new w main >/dev/null 2>&1)
ok "another machine reads the new cache" cmp -s "$T/c/store/projects/p/lines/w/target/b" target/b
finish
