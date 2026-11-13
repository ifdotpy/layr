#!/bin/bash
# ci: macos
# Caches are the contents of an overlay: a command copies them only when it asks for data of the overlay (owner
# decision 2026-11-13, docs/remotes.md). A plain `layr fetch <remote>` and a plain `layr clone` take the records, the
# lines and the base, and of a cache overlay only the records: no cache state comes, not even main's. A new line
# fetches the cache that it starts from on demand, `merge --overlays-only` fetches the caches of the line, and
# `fetch --line <line>` and `fetch --overlay <cache>` take caches too. An unreachable remote leaves the line cold.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
read id key <<<"$(env LAYR_ROOT="$T/y/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$id" --public-key "$key" >/dev/null
read zid zkey <<<"$(env LAYR_ROOT="$T/z/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$zid" --public-key "$zkey" >/dev/null
mkdir -p target; head -c 1000000 /dev/urandom > target/main.bin; l save --overlays -q
l line new side main >/dev/null 2>&1
(cd "$P/side" && echo s0 > s0.txt && l add s0.txt >/dev/null && l save -qm s0 && head -c 1000000 /dev/urandom > target/side.bin && l save --overlays -q)
l push cloud -q >/dev/null 2>&1

yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
yl clone "file://$T/archive" p --overlay base -q >/dev/null 2>&1
Y=$T/y/store/projects/p/lines
cd "$Y/main" || exit 1
states() { ls "$T/$1/store/projects/p/meta/overlays/$TG/states" 2>/dev/null | wc -l | tr -d ' '; }
chunks() { sed -n 's/.*remote get chunks: \([0-9]*\) request.*/\1/p' <<<"$1"; }

# New base history on side after the clone: the plain fetch takes it, and no cache.
(cd "$P/side" && echo s1 > s1.txt && l add s1.txt >/dev/null && l save -qm s1 && l push cloud -q >/dev/null 2>&1)
s1=$(cd "$P/side" && l rev-parse HEAD 2>/dev/null)
out=$(LAYR_TRACE=1 yl fetch origin 2>&1); code=$?;
eq "a plain fetch works" 0 "$code"
eq "it restores no cache state, not even the cache of main" 0 "$(states y)"
eq "it reads no cache data" "" "$(chunks "$out")"
has "it says that the states stay on the remote" "state(s) stay on the remote; a new line fetches the cache it starts from, fetch --overlay $TG takes them all" "$out"
ok "it takes the new base state of side" test -d "$T/y/store/projects/p/states/$s1"
out=$(yl fsck 2>&1); code=$?
eq "fsck accepts the caches that stay on the remote" 0 "$code"
has "and names them" "is on the store remote" "$out"

# A new line from main fetches main's cache on demand, once.
out=$(yl line new m1 main 2>&1)
has "a new line from main fetches its cache on demand" "fetching the cache of $TG from origin" "$out"
ok "and starts warm" cmp -s "$P/main/target/main.bin" "$Y/m1/target/main.bin"
eq "it restores only the cache of main" 1 "$(states y)"
out=$(yl line new m2 main 2>&1)
no "a second line from main fetches nothing" grep -q "fetching the cache" <<<"$out"
ok "and starts warm" cmp -s "$P/main/target/main.bin" "$Y/m2/target/main.bin"
has "fsck is clean" "0 unreachable state(s)" "$(yl fsck --overlays 2>&1)"

out=$(LAYR_TRACE=1 yl fetch origin --line side 2>&1)
no "fetch --line side reads the cache data of side" test -z "$(chunks "$out")"
eq "and restores it with the empty first state of the overlay" 3 "$(states y)"
out=$(yl line new s2 side 2>&1)
no "a new line from side fetches nothing now" grep -q "fetching the cache" <<<"$out"
ok "and starts warm" cmp -s "$P/side/target/side.bin" "$Y/s2/target/side.bin"

# fetch --overlay <cache> takes every usable cache of the overlay: a new cache of side comes.
(cd "$P/side" && head -c 1000000 /dev/urandom > target/side2.bin && l save --overlays -q && l push cloud -q >/dev/null 2>&1)
out=$(LAYR_TRACE=1 yl fetch origin 2>&1)
eq "a plain fetch leaves it on the remote" "" "$(chunks "$out")"
out=$(LAYR_TRACE=1 yl fetch origin --overlay "$TG" 2>&1); code=$?
eq "fetch --overlay <cache> works" 0 "$code"
no "it reads the cache data" test -z "$(chunks "$out")"
yl line new s3 side >/dev/null 2>&1
ok "a new line from side has the new cache" cmp -s "$P/side/target/side2.bin" "$Y/s3/target/side2.bin"

# A remote that cannot give the cache: the new line starts cold, with a warning.
(cd "$P/side" && head -c 1000000 /dev/urandom > target/side3.bin && l save --overlays -q && l push cloud -q >/dev/null 2>&1)
yl fetch origin -q >/dev/null 2>&1
mv "$T/archive" "$T/archive.away"
out=$(yl line new s4 side 2>&1); code=$?
mv "$T/archive.away" "$T/archive"
eq "a new line works while the remote is away" 0 "$code"
has "it says that the cache did not come" "caches not fetched from the store remote" "$out"
no "and starts without the new cache" test -e "$Y/s4/target/side3.bin"

# A plain clone takes no cache state either; a new line and merge --overlays-only fetch the caches they take.
zl() { env LAYR_ROOT="$T/z/store" "$LAYR" "$@"; }
zl clone "file://$T/archive" p -q >/dev/null 2>&1
eq "a plain clone works" 0 "$?"
eq "it restores no cache state" 0 "$(states z)"
Z=$T/z/store/projects/p/lines
(cd "$Z/main" && zl line new w main >/dev/null 2>&1)
ok "a new line from main in the clone starts warm" cmp -s "$P/main/target/main.bin" "$Z/w/target/main.bin"
(cd "$Z/w" && zl merge side --overlays-only -q >/dev/null 2>&1)
ok "merge --overlays-only fetches the cache of side" cmp -s "$P/side/target/side3.bin" "$Z/w/target/side3.bin"
finish
