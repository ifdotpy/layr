#!/bin/bash
# One-time setup of the comparison backends on a Linux test machine (docs/layrfs-fuse.md).
# Run as the normal (agent) user with sudo. Every system change is listed in SYSTEM_CHANGES.
# Needs: B (the bench folder on btrfs), LF (the bench helpers), LAYR (layr with the feature layrfs-fuse).
set -euo pipefail
: "${B:?}" "${LF:?}"
log() { echo "$*" | tee -a "$B/SYSTEM_CHANGES"; }
me=$(id -u); mygid=$(id -g)
mkdir -p "$B"

# 1. The service user that owns the store (the tamper model): no login, no sudo.
if ! id layr >/dev/null 2>&1; then
    sudo useradd --system --create-home --home-dir /home/layr --shell /usr/sbin/nologin layr
    log "useradd --system layr (home /home/layr)"
fi
sudo chmod 0711 /home/layr
sudo -u layr mkdir -p /home/layr/store /home/layr/lines /home/layr/run
sudo -u layr chmod 0700 /home/layr/store
sudo -u layr chmod 0755 /home/layr/lines /home/layr/run
# The binaries must be readable by the service user: the bench helpers, and layr with the
# feature layrfs-fuse (it serves the views).
sudo install -m 0755 -o layr -g layr "$LF" /home/layr/layrfs-fuse
log "install /home/layr/layrfs-fuse (owned by layr)"
if [ -n "${LAYR:-}" ]; then
    sudo install -m 0755 -o layr -g layr "$LAYR" /home/layr/layr
    log "install /home/layr/layr (owned by layr)"
fi

# 2. allow_other for user mounts (an agent user reads the mount of the service user).
if ! grep -q '^user_allow_other' /etc/fuse.conf; then
    echo user_allow_other | sudo tee -a /etc/fuse.conf >/dev/null
    log "/etc/fuse.conf: user_allow_other"
fi

# 3. Btrfs backend: subvolumes in the bench folder (/home is btrfs).
mkdir -p "$B/btrfs/lines" "$B/btrfs/states"

# 4. OverlayFS backend: an XFS file system in a loop file (reflinks; the data of the image does not use
# btrfs copy-on-write or compression).
if ! mountpoint -q "$B/xfs"; then
    if [ ! -f "$B/xfs.img" ]; then
        touch "$B/xfs.img"; chattr +C "$B/xfs.img"; truncate -s 60G "$B/xfs.img"
        sudo mkfs.xfs -q -m reflink=1 "$B/xfs.img"
        log "mkfs.xfs $B/xfs.img (60 GiB sparse, nodatacow)"
    fi
    mkdir -p "$B/xfs"
    sudo mount -o loop,noatime "$B/xfs.img" "$B/xfs"
    sudo chown "$me:$mygid" "$B/xfs"
    log "mount -o loop $B/xfs.img $B/xfs (chown of the empty root to $me)"
fi
echo setup done
