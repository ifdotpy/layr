#!/bin/bash
# Make a btrfs file system on a loop file and copy a sample tree into the line `line`.
# Usage (root): setup.sh <source folder>
# Environment: W (work folder, default /root/ovl-bench), SIZE (image size, default 8G).
# Writes: $W/fs.img, the mount $W/mnt, $W/all.txt (every regular file of the tree) and
# $W/ovl-<n>.txt (n files picked at random over the tree, in tree order).
set -eu
src=${1:?usage: setup.sh <source folder>}
W=${W:-/root/ovl-bench}
M=$W/mnt
[ -e "$W" ] && { echo "$W exists" >&2; exit 1; }
mkdir -p "$M"
truncate -s "${SIZE:-8G}" "$W/fs.img"
mkfs.btrfs -q -f "$W/fs.img"
mount -o loop,user_subvol_rm_allowed "$W/fs.img" "$M"
mkdir "$M/states" "$M/work"
btrfs -q subvolume create "$M/line"
# The tree without version control folders and installed packages.
tar -C "$src" --exclude=.git --exclude=node_modules -cf - . | tar -C "$M/line" -xf -
cd "$M/line" || exit 1
find . -type f | sed 's|^\./||' | LC_ALL=C sort >"$W/all.txt"
for n in 10 1000 10000; do
    shuf -n "$n" --random-source=<(yes layr-overlays) "$W/all.txt" | LC_ALL=C sort >"$W/ovl-$n.txt"
done
btrfs -q subvolume snapshot -r "$M/line" "$M/states/full"
btrfs -q filesystem sync "$M"
echo "files $(wc -l <"$W/all.txt") folders $(find . -type d | wc -l) bytes $(du -sb . | cut -f1)"
