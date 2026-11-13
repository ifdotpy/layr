#!/bin/bash
# ci: macos
# Eviction of cache states (docs/cache-overlays.md, "Eviction"): not every machine must store all data; a store
# remote must. With `gc.evict on`, `gc --repair` removes the local tree of a cache state that a store remote holds
# and that no working folder uses now, and says how many bytes went. A new line that takes that cache fetches it back
# from the remote. An unpushed cache, and the newest cache of a line with a working folder, stay.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/overlays/$TG
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
mkdir -p target; head -c 1000000 /dev/urandom > target/main.bin; l save --overlays -q
l overlay set "$TG" --history 10 >/dev/null
r1=machine:$(uuidgen | tr 'A-Z' 'a-z')
r2=machine:$(uuidgen | tr 'A-Z' 'a-z')
l overlay readers "$TG" "$r1" "$r2" >/dev/null 2>&1

head_of() { python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT head FROM lines WHERE name=?", (sys.argv[2],)).fetchone()[0])' "$O/meta/db/layr.sqlite" "$1"; }
present() { python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT present FROM states WHERE id=?", (sys.argv[2],)).fetchone()[0])' "$O/meta/db/layr.sqlite" "$1"; }
kb() { du -sk "$O/states" | cut -f1; }

# side: the cache c1 is saved with the state s0 and pushed. Then side.bin goes from the folder, so only c1 holds it.
l line new side main >/dev/null 2>&1
cd "$P/side" || exit 1
echo s0 > s0.txt; l add s0.txt >/dev/null; l save -qm s0
s0=$(l rev-parse head)
head -c 2000000 /dev/urandom > target/side.bin; l save --overlays -q
c1=$(head_of side)
l push cloud -q >/dev/null 2>&1
# Two caches after the push: c2 (not pushed, no line shows it) and c3 (the newest cache of side).
cp target/side.bin "$T/side.bin"; rm target/side.bin
echo s1 > s1.txt; l add s1.txt >/dev/null; l save -qm s1
head -c 500000 /dev/urandom > target/side2.bin; l save --overlays -q
c2=$(head_of side)
echo s2 > s2.txt; l add s2.txt >/dev/null; l save -qm s2
head -c 500000 /dev/urandom > target/side3.bin; l save --overlays -q
c3=$(head_of side)
cmain=$(head_of main)
cd "$P/main" || exit 1

out=$(l gc --repair 2>&1)
no "with gc.evict off, gc evicts no cache" grep -q "cache state(s)" <<<"$out"
eq "the pushed cache stays with gc.evict off" 1 "$(present "$c1")"

l config gc.evict on >/dev/null
before=$(kb)
out=$(l gc --repair 2>&1); code=$?
eq "gc --repair works" 0 "$code"
has "it evicts the pushed cache and says how much it freed" "1 cache state(s) that a store remote holds" "$out"
no "it frees more than nothing" grep -q "cache state(s) that a store remote holds, at least 0 B freed" <<<"$out"
eq "the pushed cache has no tree here" 0 "$(present "$c1")"
after=$(kb)
ok "the cache folder of the overlay shrinks by about the 2 MB that only c1 held ($before KB to $after KB)" test $((before - after)) -ge 1500
eq "a cache that no push sent stays" 1 "$(present "$c2")"
eq "the newest cache of a line with a working folder stays" 1 "$(present "$c3")"
eq "the pushed cache of main, which main's folder uses, stays" 1 "$(present "$cmain")"
ok "fsck accepts the evicted cache" l fsck
out=$(l gc --repair 2>&1)
no "a second gc evicts nothing more" grep -q "cache state(s) that a store remote holds" <<<"$out"

# The remote is away: the evicted cache cannot come, and the new line starts cold with a warning.
mv "$T/archive" "$T/archive.away"
out=$(l line new cold "$s0" 2>&1); code=$?
mv "$T/archive.away" "$T/archive"
eq "a new line works while the remote is away" 0 "$code"
has "it says that the cache did not come" "caches not fetched from the store remote" "$out"
no "and starts cold" test -e "$P/cold/target/side.bin"
rm -f "$LAYR_ROOT"/projects/p/meta/remote-stores/*/on-demand.json

# Key epochs: a reader removal starts a new epoch at the next push, and that push deletes the trees of the old epoch.
epochs() { find "$T/archive/overlays" -path '*/epochs/*' -name '*.tree.age' | sed 's|.*/epochs/\([^/]*\)/.*|\1|' | sort -u | wc -l | tr -d ' '; }
cd "$P/side" || exit 1
echo s3 > s3.txt; l add s3.txt >/dev/null; l save -qm s3
s3=$(l rev-parse head)
head -c 1000000 /dev/urandom > target/side4.bin; l save --overlays -q
c4=$(head_of side)
cp target/side4.bin "$T/side4.bin"
l push cloud -q >/dev/null 2>&1
rm target/side4.bin
echo s4 > s4.txt; l add s4.txt >/dev/null; l save -qm s4
head -c 1000 /dev/urandom > target/side5.bin; l save --overlays -q
cd "$P/main" || exit 1
l overlay readers "$TG" "$r2" >/dev/null 2>&1
l gc --repair -q >/dev/null 2>&1
eq "while a key change waits, gc evicts no cache" 1 "$(present "$c4")"
eq "one epoch has cache states before the key change" 1 "$(epochs)"
out=$(l push cloud 2>&1); code=$?
eq "the push of the key change works" 0 "$code"
has "it fetches the evicted cache back before the key change" "fetched 1 evicted cache state(s) back before the key change" "$out"
has "and deletes the old epoch" "of older key epochs" "$out"
eq "only the new epoch has cache states" 1 "$(epochs)"
eq "the evicted cache is here again" 1 "$(present "$c1")"
out=$(l gc --repair 2>&1)
has "the push to the new epoch counts: gc evicts again" "cache state(s) that a store remote holds" "$out"
eq "c1 is evicted" 0 "$(present "$c1")"
eq "c4 is evicted" 0 "$(present "$c4")"

# Any read of an evicted tree restores it from a store remote of the project.
l overlay show "$TG" "$c1:side.bin" > "$T/read.bin" 2>/dev/null
ok "a read of the evicted cache restores it from the new epoch" cmp -s "$T/side.bin" "$T/read.bin"
eq "and the cache has its tree again" 1 "$(present "$c1")"

# A new line from s3 takes c4: it fetches it back from the remote on demand.
out=$(l line new after "$s3" 2>&1)
has "a new line from s3 fetches the evicted cache" "fetching the cache of $TG from cloud" "$out"
eq "the cache has its tree again" 1 "$(present "$c4")"
ok "and the new line starts warm with it" cmp -s "$T/side4.bin" "$P/after/target/side4.bin"
no "and without the later caches" test -e "$P/after/target/side5.bin"
l line delete -D after >/dev/null 2>&1
l gc --repair -q >/dev/null 2>&1
eq "the cache that a deleted line showed stays: an undelete reads it" 1 "$(present "$c4")"
finish
