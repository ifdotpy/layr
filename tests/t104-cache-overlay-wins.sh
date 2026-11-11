#!/bin/bash
# ci: overlayfs macos-plain
# A new line takes the newest cache overlay state, also when an older cache of the state
# (`states/<id>@<folder>`, the mechanism before cache overlays) is there: protected main saves its
# caches twice at the same head; the state keeps its first cache, the overlay gets the second.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main >/dev/null
head=$(l rev-parse main)
rm -rf target; mkdir -p target/debug; echo first > target/debug/x
l save --caches -q
echo second > target/debug/x
l save --caches -q
eq "main stays at its head" "$head" "$(l rev-parse main)"
has "the overlay has the second cache" "saved " "$(l overlay list | grep $TG)"

l line new from-line main >/dev/null
eq "a line from main takes the newest cache" second "$(cat $P/from-line/target/debug/x 2>&1)"
l line new from-state "$head" >/dev/null
eq "a line from the head state takes the newest cache" second "$(cat $P/from-state/target/debug/x 2>&1)"

finish
