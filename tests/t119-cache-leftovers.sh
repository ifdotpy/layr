#!/bin/bash
# ci: macos overlayfs
# A cache swap leftover (<cache>.layr-old-<id>) is reported by gc and removed by gc --repair. A
# folder with another name stays.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init lf --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/lf/lines
cd $P/main
l config regenerable '["target"]'
l line new -q w
cd $P/w
mkdir target.layr-old-abc1234 target.mine
echo old > target.layr-old-abc1234/out
echo keep > target.mine/keep
report=$(l gc 2>&1)
has "gc reports the leftover with its name" "target.layr-old-abc1234" "$report"
has "gc reports the leftover in its line" "w: target.layr-old-abc1234" "$report"
ok "gc alone keeps the leftover" test -d target.layr-old-abc1234
l gc --repair -q >/dev/null 2>&1
no "gc --repair removes the leftover" test -e target.layr-old-abc1234
ok "gc --repair keeps a folder with another name" test -f target.mine/keep
finish
