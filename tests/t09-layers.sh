#!/bin/bash
# Regenerable layers: folders kept as nested subvolumes, outside automatic states and backups;
# carried for a live move with --layers.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd $P/main
l layer add target
ok "the layer folder is a subvolume" sh -c "btrfs subvolume show $P/main/target >/dev/null"
l branch -q w
cd $P/w
ok "a new line gets the layer as a subvolume" sh -c "btrfs subvolume show $P/w/target >/dev/null"
head -c 20000000 /dev/urandom > target/big.bin; echo src >> notes.txt
l save -q
eq "automatic states do not hold the layer content" "" "$(l show HEAD@{0}:target | tail -n +3)"
eq "status ignores the layer" " M notes.txt" "$(l status -s)"
rm -rf target; mkdir target; echo rebuilt > target/out
eq "a tool replaced the layer with a plain folder" "plain" "$(l layer list | awk '{print $2}')"
l save -q --turn-end
eq "the end of the turn makes it a layer again" "layer" "$(l layer list | awk '{print $2}')"
eq "and keeps its content" "rebuilt" "$(cat target/out)"
# A live move with warm build output. The first sync sends the state without layers; the next
# sync with --layers must still send them.
new_fs $T/b 4G
head -c 1000000 /dev/urandom > target/warm.bin
l save -q --layers
l sync -q --auto -- env LAYR_ROOT=$T/b/store $LAYR serve-peer g
l sync -q --auto --layers -- env LAYR_ROOT=$T/b/store $LAYR serve-peer g
env LAYR_ROOT=$T/b/store $LAYR --project g adopt w >/dev/null
eq "the moved line has the build output" "$(md5sum < target/warm.bin)" "$(md5sum < $T/b/store/projects/g/lines/w/target/warm.bin)"
ok "and it is a layer there" sh -c "btrfs subvolume show $T/b/store/projects/g/lines/w/target >/dev/null"
# Backups leave layers out.
l backup -q $T/bk
eq "backups hold no layer streams" "0" "$(ls $T/bk/*/chain-0001 | grep -c '@')"
finish
