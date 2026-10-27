#!/bin/bash
# Cache overlays (docs/cache-overlays.md): `regenerable` folders become folder overlays; a line has
# caches, so new lines from a line start warm; merge and `cache pull` bring caches between lines;
# `gc lru:<size>` limits them. One user, so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/overlays
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
states() { ls "$O/$1/states" | wc -l | tr -d ' '; }

# 1. save --caches makes the cache overlay of the folder and saves a state for the line.
l line new rel main >/dev/null
cd $P/rel
rm -rf target; mkdir -p target/debug; echo bin1 > target/debug/x
l cache repair >/dev/null
out=$(l save --caches)
has "save --caches names the cache overlay" "target: overlay cache.target" "$out"
has "the overlay is a folder overlay with the cache properties" "folder=target history=last quiet merge=union-theirs" "$(l overlay list)"
eq "cache list keeps its format" "$(printf 'target\tcache')" "$(l cache list)"
has "cache list -v shows the saved overlay state" "overlay cache.target" "$(l cache list -v)"
eq "status does not show the cache" "" "$(l status --short)"
no "overlay use refuses a folder overlay" l overlay use --add cache.target
no "overlay move refuses a folder overlay" l overlay move cache.target notes.txt

# 2. A new line from the line starts warm (dogfood item 10), and its cache is its own.
l line new warm rel >/dev/null
eq "a new line from the line has the cache" bin1 "$(cat $P/warm/target/debug/x)"
echo changed > $P/warm/target/debug/x
eq "the copy is separate" bin1 "$(cat target/debug/x)"
echo changed > $P/warm/target/debug/x

# 3. A new line from a named state of the line takes that line's cache (dogfood item 21).
echo more >> notes.txt
l save -a -m "a named state" -q
named=$(l rev-parse head)
l line new fromstate $named >/dev/null
eq "a new line from a named state of the line has the cache" bin1 "$(cat $P/fromstate/target/debug/x 2>/dev/null)"

# 4. history=last: each line keeps its newest cache state.
echo bin2 > target/debug/x
l save --caches -q
echo bin3 > target/debug/x
l save --caches -q
eq "the first state and the heads of rel, warm and fromstate stay" 3 "$(states cache.target)"
l line new warm2 rel >/dev/null
eq "a new line takes the newest cache" bin3 "$(cat $P/warm2/target/debug/x)"

l line delete -D fromstate >/dev/null 2>&1
l gc -q >/dev/null 2>&1
eq "gc drops the cache of a deleted line" 3 "$(states cache.target)"

# 5. merge of a line unions its cache into the target; cache pull takes another line's cache.
cd $P/warm
echo w > target/debug/w
l save --caches -q
cd $P/main
out=$(l merge warm 2>&1)
has "the merge brings the cache" "Merged cache target" "$out"
eq "main has the merged line's files" "changed w" "$(cat target/debug/x target/debug/w | tr '\n' ' ' | sed 's/ $//')"
out=$(l cache pull rel)
has "cache pull names the folder" "target:" "$out"
eq "the incoming file wins" bin3 "$(cat target/debug/x)"
eq "and the line's other files stay" w "$(cat target/debug/w)"
eq "a second pull has nothing new" "no newer caches in rel" "$(l cache pull rel)"
eq "status stays clean" "" "$(l status --short)"

# 6. gc lru:<size> trims the cache folders of the lines, oldest files first.
head -c 300000 /dev/zero > $P/rel/target/debug/big
head -c 150000 /dev/zero > $P/rel/target/debug/mid
touch -t 200001010000 $P/rel/target/debug/x
touch -t 200101010000 $P/rel/target/debug/mid
ok "set a size limit" l overlay set cache.target --gc lru:400K
l gc -q >/dev/null 2>&1
no "the oldest file went" test -e $P/rel/target/debug/x
no "the next oldest went until the folder fits" test -e $P/rel/target/debug/mid
ok "the newest file stayed" test -e $P/rel/target/debug/big
ok "a line under the limit keeps its files" test -e $P/main/target/debug/w

# 7. A folder overlay made by hand.
ok "overlay create --folder" l overlay create deps --folder vendor/cache -q
has "it is a follow folder overlay" "folder=vendor/cache" "$(l overlay list)"
no "a second overlay for the same folder fails" l overlay create deps2 --folder vendor/cache
no "a folder overlay is not shared" l overlay create deps3 --folder x --mode shared
no "an absolute folder fails" l overlay create deps4 --folder /tmp

# 8. A folder that .layrattributes no longer marks regenerable is not taken.
cd $P/main
ok "main has a cache to give" test -e target/debug/w
echo 'target -regenerable' > .layrattributes
l save -a -m "no cache" -q
l line new cold main >/dev/null
no "a new line does not take it" test -e $P/cold/target/debug/w

finish
