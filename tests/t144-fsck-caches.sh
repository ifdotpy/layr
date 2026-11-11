#!/bin/bash
# ci: macos overlayfs
# `layr fsck --glasses` reports the cache data that nothing keeps, and the operations that make cache data
# dead delete it: after saves on two platforms and a line delete the report is 0, with no gc.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target
for i in 1 2 3; do echo m$i > target/out; l save --glasses -q; done
echo other > target/out; LAYR_TEST_PLATFORM=plan9-mips l save --glasses -q
l line new x main >/dev/null 2>&1
cd $P/x && echo x1 > target/out && l save --glasses -q && echo x2 > target/out && l save --glasses -q
cd $P/main && l line delete -D x >/dev/null
out=$(l fsck --glasses 2>&1); rc=$?
has "after saves and a line delete nothing is unreachable" "unreachable cache data: 0 item(s), 0 bytes" "$out"
eq "fsck --glasses exits 0" 0 "$rc"

# Old states that a longer history kept become unreachable when the history gets shorter; the next
# operation deletes them.
l overlay set $TG --history all >/dev/null
for i in 4 5 6; do echo m$i > target/out; l save --glasses -q; done
l config overlay.$TG.history 1 >/dev/null
out=$(l fsck --glasses 2>&1); rc=$?
eq "fsck --glasses exits 1 when it finds some" 1 "$rc"
has "it names the overlay and the states" "overlay $TG: 3 unreachable state(s)" "$out"
echo m7 > target/out; l save --glasses -q
has "the next save deletes them" "unreachable cache data: 0 item(s), 0 bytes" "$(l fsck --glasses 2>&1)"
finish
