#!/bin/bash
# A persistent btrfs loop mount, independent of the VM root filesystem.
set -euo pipefail
test "$(id -u)" = 0
image=/var/lib/layr-runner/store.img
mountpoint=/var/lib/layr
if [ "$(stat -f -c %T /var/lib)" = btrfs ]; then
    mkdir -p "$mountpoint"
    exit
fi
mkdir -p /var/lib/layr-runner "$mountpoint"
if [ ! -f "$image" ]; then
    truncate -s 32G "$image"
    mkfs.btrfs -q "$image"
fi
if ! grep -Fq "$image $mountpoint " /etc/fstab; then
    echo "$image $mountpoint btrfs loop,user_subvol_rm_allowed 0 0" >> /etc/fstab
fi
mountpoint -q "$mountpoint" || mount "$mountpoint"
