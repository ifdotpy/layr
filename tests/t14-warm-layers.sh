#!/bin/bash
# Warm layers: a new line takes the build output of its nearest ancestor that has one; build
# output of another user only from a protected line; layers come only from an explicit
# save --layers, never from a check; gc keeps the layers of line heads and of the newest states.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1 layr-u2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=w LAYR_LINE=$line $T/layr "$@"; }
L=$T/layr
sample $T/src
l init w --from $T/src --owner layr-admin -q >/dev/null
P=$LAYR_ROOT/projects/w/lines
l --project w access grant layr-u1 writer
l --project w access grant layr-u2 maintainer
(cd $P/main && l layer add target)
# The "build" of the check: a counter shows whether it started from earlier output.
l --project w config check.build 'n=$(cat target/count 2>/dev/null || echo 0); echo $((n+1)) > target/count; echo built > target/out; head -c 1048576 /dev/zero > target/big.bin'
l --project w protect main --check build
cd /tmp || exit 1
tip() { in_line $1 $2 rev-parse HEAD; }

# 1. Before any build a new line has an empty layer.
as layr-u1 $L --project w branch -q a1 main
eq "an empty layer before any build" "" "$(ls -A $P/a1/target)"
ok "the layer is a subvolume" sh -c "btrfs subvolume show $P/a1/target >/dev/null"

# 2. The check of a merge into main builds in its copy, but layr does not keep that output.
cd $P/a1 && as layr-u1 sh -c "echo a > a.txt && $L add a.txt && $L commit -qm a" && cd /tmp
ok "merge with its check" in_line layr-u2 main merge -q a1 --expect $(tip layr-u1 a1)
no "a passing check keeps no build output" sh -c "ls $LAYR_ROOT/projects/w/states | grep -q @target"

# 3. The owner of main builds there and keeps the output on purpose: the head of main gets it.
as layr-admin sh -c "cd $P/main && echo 1 > target/count && echo built > target/out && head -c 1048576 /dev/zero > target/big.bin"
head_before=$(tip layr-admin main)
ok "save --layers on a clean main" in_line layr-admin main save -q --layers
eq "the head of main did not move" "$head_before" "$(tip layr-admin main)"
has "the head of main keeps the build output" "@target" "$(ls $LAYR_ROOT/projects/w/states)"

# 4. A new line from main starts warm, owns its copy, and shares the blocks.
as layr-u1 $L --project w branch -q a2 main
eq "a new line starts warm" "built" "$(cat $P/a2/target/out)"
eq "its build output belongs to the line owner" "layr-u1" "$(stat -c %U $P/a2/target/big.bin)"
eq "the copy has no blocks of its own" "0" "$(btrfs filesystem du --raw $P/a2/target/big.bin | awk 'NR==2{print $2}')"
ok "the owner can change it" as layr-u1 sh -c "echo more >> $P/a2/target/out"

# 5. The next check starts warm, and its output again is not kept.
cd $P/a2 && as layr-u1 sh -c "echo b > b.txt && $L add b.txt && $L commit -qm b" && cd /tmp
ok "second merge" in_line layr-u2 main merge -q a2 --expect $(tip layr-u1 a2)
as layr-u1 $L --project w branch -q a3 main
eq "a new line gets the saved output, not the check's" "1" "$(cat $P/a3/target/count)"

# 6. A user's own build output goes to that user's lines only; a line that is not protected
#    saves a new state for it, so the head it shares with main gets nothing.
cd $P/a3 && as layr-u1 sh -c "echo mine > target/mine && $L save -q --layers" && cd /tmp
as layr-u1 $L --project w branch -q c1 main
no "the head of main does not take the layers of another line" test -e $P/c1/target/mine
in_line layr-u1 a3 branch -q b1 'HEAD@{0}'
eq "to the same user it goes" "mine" "$(cat $P/b1/target/mine)"
in_line layr-u2 a3 branch -q b2 'HEAD@{0}'
no "to another user it does not" test -e $P/b2/target/mine
eq "who gets the shared output of the nearest ancestor instead" "1" "$(cat $P/b2/target/count)"

# 7. gc keeps the layers of line heads and of the newest states only.
ok "main keeps its build output at its new head" in_line layr-admin main save -q --layers
as layr-u2 $L --project w branch -q -D a1
l --project w config retention.layers 0
n_before=$(ls $LAYR_ROOT/projects/w/states | grep -c @)
l --project w gc -q
n_after=$(ls $LAYR_ROOT/projects/w/states | grep -c @)
ok "gc deletes the layers of old states" test "$n_after" -lt "$n_before"
as layr-u1 $L --project w branch -q a4 main
eq "the head of main keeps its layer" "1" "$(cat $P/a4/target/count)"
finish
