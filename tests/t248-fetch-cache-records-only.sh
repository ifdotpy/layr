#!/bin/bash
# ci: macos
# A plain `layr fetch <remote>` takes the records, the lines and the base, and of a cache overlay only the records and
# the caches that new lines from main, the protected lines and the save-back lines start from (docs/remotes.md): the
# caches of the other lines stay on the remote. `fetch --line <line>` and `fetch --overlay <cache>` take them, and a
# plain clone still starts warm (t154).
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
mkdir -p target; head -c 1000000 /dev/urandom > target/main.bin; l save --overlays -q
l line new side main >/dev/null 2>&1
(cd "$P/side" && echo s0 > s0.txt && l add s0.txt >/dev/null && l save -qm s0 && head -c 1000000 /dev/urandom > target/side.bin && l save --overlays -q)
l push cloud -q >/dev/null 2>&1

yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
yl clone "file://$T/archive" p --overlay base -q >/dev/null 2>&1
Y=$T/y/store/projects/p/lines
cd "$Y/main" || exit 1
states() { ls "$T/y/store/projects/p/meta/overlays/$TG/states" 2>/dev/null | wc -l | tr -d ' '; }
chunks() { sed -n 's/.*remote get chunks: \([0-9]*\) request.*/\1/p' <<<"$1"; }

# New base history on side after the clone: the plain fetch takes it.
(cd "$P/side" && echo s1 > s1.txt && l add s1.txt >/dev/null && l save -qm s1 && l push cloud -q >/dev/null 2>&1)
s1=$(cd "$P/side" && l rev-parse HEAD 2>/dev/null)
out=$(LAYR_TRACE=1 yl fetch origin 2>&1); code=$?
eq "a plain fetch works" 0 "$code"
eq "it restores the cache of main and the empty first state, not the cache of side" 2 "$(states)"
has "it says that the cache of side stays on the remote" "1 cache state(s) stay on the remote; fetch --overlay $TG takes them" "$out"
ok "it takes the new base state of side" test -d "$T/y/store/projects/p/states/$s1"
yl line new m1 main >/dev/null 2>&1
ok "a new line from main starts warm" cmp -s "$P/main/target/main.bin" "$Y/m1/target/main.bin"
yl line new s1 side >/dev/null 2>&1
ok "a new line from side works" test -f "$Y/s1/s0.txt"
no "and starts cold" test -e "$Y/s1/target/side.bin"
has "fsck is clean" "0 unreachable state(s)" "$(yl fsck --overlays 2>&1)"

out=$(LAYR_TRACE=1 yl fetch origin --line side 2>&1)
no "fetch --line side reads the cache data of side" test -z "$(chunks "$out")"
eq "and restores it" 3 "$(states)"
yl line new s2 side >/dev/null 2>&1
ok "a new line from side starts warm now" cmp -s "$P/side/target/side.bin" "$Y/s2/target/side.bin"

# fetch --overlay <cache> takes every usable cache of the overlay: a new cache of side comes.
(cd "$P/side" && head -c 1000000 /dev/urandom > target/side2.bin && l save --overlays -q && l push cloud -q >/dev/null 2>&1)
out=$(LAYR_TRACE=1 yl fetch origin 2>&1)
eq "a plain fetch leaves it on the remote" "" "$(chunks "$out")"
out=$(LAYR_TRACE=1 yl fetch origin --overlay "$TG" 2>&1); code=$?
eq "fetch --overlay <cache> works" 0 "$code"
no "it reads the cache data" test -z "$(chunks "$out")"
yl line new s3 side >/dev/null 2>&1
ok "a new line from side has the new cache" cmp -s "$P/side/target/side2.bin" "$Y/s3/target/side2.bin"
finish
