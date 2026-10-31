#!/bin/bash
# A tool deletes a cache and makes a plain folder (cargo clean). The next plain save makes it a
# cache again first: its files never enter a state.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd $P/main || exit 1
l config regenerable '["target"]'
l line new -q w >/dev/null
cd $P/w || exit 1
echo one >> notes.txt
l save -q
rm -rf target; mkdir target; echo big > target/x
echo two >> notes.txt
l save -q
eq "the state has no cache content" "" "$(l show head@{0}:target | tail -n +3)"
eq "the state has the change" 1 "$(l show head@{0}:notes.txt | grep -c two)"
eq "the file is still in the folder" big "$(cat target/x)"
eq "status ignores the cache" " M notes.txt" "$(l status -s)"
# Again, with the cache already a layer in an earlier state: replace it twice.
rm -rf target; mkdir target; echo big2 > target/y
echo three >> notes.txt
l save -q
eq "a second time: no cache content" "" "$(l show head@{0}:target | tail -n +3)"
eq "the second file is kept" big2 "$(cat target/y)"
finish
