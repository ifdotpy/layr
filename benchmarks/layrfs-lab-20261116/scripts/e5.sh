#!/bin/bash
# E5: which Btrfs steps of a line work without root (LayrFS steps run as the users layr and igos3).
. ~/lfx/lab/lab.sh
cd $B/btrfs
ls -ld . lines states linux-base
try() { if "$@" >/tmp/e5.out 2>&1; then echo "OK    $*"; else echo "FAIL  $*: $(tail -1 /tmp/e5.out)"; fi; }
try btrfs subvolume snapshot linux-base lines/e5-rw
try btrfs subvolume snapshot linux-base /home/igos3/e5-rw-home
try btrfs subvolume snapshot -r lines/e5-rw states/e5-ro
try sh -c "btrfs send -q --no-data -p states/linux-base states/e5-ro >/dev/null"
try btrfs subvolume delete states/e5-ro
try btrfs subvolume delete lines/e5-rw
try btrfs subvolume delete /home/igos3/e5-rw-home
grep -o 'user_subvol_rm_allowed' /proc/mounts | head -1 || echo "no user_subvol_rm_allowed"
for p in states/e5-ro lines/e5-rw /home/igos3/e5-rw-home; do [ -e $p ] && sudo btrfs subvolume delete $p >/dev/null || true; done
