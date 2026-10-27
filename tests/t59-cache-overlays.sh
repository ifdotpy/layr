#!/bin/bash
# Cache overlays (docs/cache-overlays.md): `regenerable` folders become folder overlays; a line has
# caches, so new lines from a line start warm; a merge brings caches back; `gc lru:<size>` limits
# them; `overlay list` and `overlay repair` replace the removed `cache` commands. One user, so it
# runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/overlays
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
states() { ls "$O/$1/states" | wc -l | tr -d ' '; }

# 1. save --caches makes the cache overlay of the folder and saves a state for the line. Its
#    readers are the machines the project trusts, not every user.
self_id=$(l remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
l line new rel main >/dev/null
cd $P/rel
rm -rf target; mkdir -p target/debug; echo bin1 > target/debug/x
out=$(l save --caches)
has "save --caches names the cache overlay" "target: overlay cache.target" "$out"
has "the overlay is a folder overlay with the cache properties" "folder=target history=last quiet merge=union-theirs" "$(l overlay list)"
row=$(l overlay list | grep cache.target)
eq "overlay list shows the state of the folder" "cache" "$(echo "$row" | cut -f6)"
has "overlay list shows the size of the cache" "B" "$(echo "$row" | cut -f7)"
has "overlay list shows the saved overlay state" "saved " "$(echo "$row" | cut -f8)"
eq "status does not show the cache" "" "$(l status --short)"
readers=$(l overlay readers cache.target)
has "this machine reads the cache overlay" "machine:$self_id" "$readers"
eq "not every user" "" "$(echo "$readers" | grep -x '\*')"
new_fs $T/m2 1G
other=$(env LAYR_ROOT=$T/m2/store $LAYR remote self --json)
other_id=$(echo "$other" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
other_key=$(echo "$other" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')
l remote add test-peer ssh://test/p >/dev/null
ok "trust another machine" l remote trust test-peer $other_id --public-key $other_key
l save --caches -q
has "the next save --caches adds it to the readers" "machine:$other_id" "$(l overlay readers cache.target)"
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

# 5. merge of a line unions its cache into the target.
cd $P/warm
echo w > target/debug/w
l save --caches -q
cd $P/main
out=$(l merge warm 2>&1)
has "the merge brings the cache" "Merged cache target" "$out"
eq "main has the merged line's files" "changed w" "$(cat target/debug/x target/debug/w | tr '\n' ' ' | sed 's/ $//')"
eq "the incoming file wins" "changed" "$(cat target/debug/x)"
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

# 8b. The removed cache commands name their replacements; overlay repair makes a cache again.
has "cache add names its replacement" "layr cache add was removed. Use 'regenerable' in .layrattributes" "$(l cache add x 2>&1)"
has "cache list names overlay list" "layr cache list was removed. Use 'layr overlay list'." "$(l cache list -v 2>&1)"
has "cache pull names the line and merge commands" "layr cache pull was removed. Use 'layr line new <name> <line>' or 'layr merge <line>'." "$(l cache pull rel 2>&1)"
has "cache repair names overlay repair" "layr cache repair was removed. Use 'layr overlay repair <overlay>'." "$(l cache repair 2>&1)"
has "cache remove names -regenerable" "layr cache remove was removed. Use '-regenerable' in .layrattributes" "$(l cache remove target 2>&1)"
no "each removed form fails" l cache list
cd $P/rel
rm -rf target; mkdir -p target/debug; echo tool > target/debug/x
eq "overlay repair makes the plain folder a cache again" "target: a cache again" "$(l overlay repair cache.target)"
eq "and keeps its files" "tool" "$(cat target/debug/x)"
has "repair of a folder that is no cache path fails" "is no cache path" "$(l overlay repair deps 2>&1 || true)"
cd $P/main

# 9. A protected line takes caches only with a merge that met its protection.
cd $P/main
l config check.unit 'test ! -e fail.txt'
l protect main --check unit --no-expect >/dev/null
l line new pm main >/dev/null
cd $P/pm
echo pm1 > target/debug/pm1
l save --caches -q
cd $P/main
out=$(l merge pm 2>&1)
has "an up-to-date merge without a passing check carries no cache" "not merged" "$out"
no "so main has no file of it" test -e target/debug/pm1
cd $P/pm
echo code >> notes.txt
l save -a -m "pm code" -q
l save --caches -q
cd $P/main
out=$(l merge pm 2>&1)
has "a merge whose check passed carries the cache" "Merged cache target" "$out"
ok "main has the file" test -e target/debug/pm1
l protect main --check unit --approvals 1 --no-expect >/dev/null
cd $P/pm
echo pm2 > target/debug/pm2
l save --caches -q
cd $P/main
out=$(l merge pm 2>&1)
has "a merge that lacks the approval carries no cache" "not merged" "$out"
no "so main has no new file" test -e target/debug/pm2
cd $P/pm
echo more >> notes.txt
l save -a -m "pm code 2" -q
l save --caches -q
cd $P/main
# Root bypasses approvals (the Linux suite runs as root): the merge then succeeds, but it did not
# meet the protection, so it still carries no cache.
if [ "$(id -u)" = 0 ]; then
    has "a root merge without the approval carries no cache" "not merged" "$(l merge pm 2>&1)"
else
    no "a merge that lacks the approval is refused" l merge pm
fi
no "and carries no cache" test -e target/debug/pm2
l protect main --remove >/dev/null

# 8. A folder that .layrattributes no longer marks regenerable is not taken.
cd $P/main
ok "main has a cache to give" test -e target/debug/w
echo 'target -regenerable' > .layrattributes
l save -a -m "no cache" -q
l line new cold main >/dev/null
no "a new line does not take it" test -e $P/cold/target/debug/w

finish
