#!/bin/bash
# ci: macos overlayfs
# `line delete` deletes the cache states of the line at once, with no gc (docs/cache-overlays.md): the
# states that stay are the heads of the other lines' cache overlay lines, the states that lines show,
# and the empty first state.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/overlays/$TG/states
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target; echo m > target/out; l save --caches -q
kept=$(ls $O | wc -l | tr -d ' ')
l line new x main >/dev/null 2>&1
cd $P/x || exit 1
echo x1 > target/out; l save --caches -q
echo x2 > target/out; l save --caches -q
no "the line has its own cache states" test "$(ls $O | wc -l | tr -d ' ')" -eq "$kept"
cd $P/main || exit 1
l line delete -D x >/dev/null
eq "line delete deletes the line's cache states at once" "$kept" "$(ls $O | wc -l | tr -d ' ')"
eq "main keeps its cache" m "$(cat target/out)"
l line new y main >/dev/null 2>&1
eq "a new line still takes main's cache" m "$(cat $P/y/target/out 2>/dev/null)"
finish
