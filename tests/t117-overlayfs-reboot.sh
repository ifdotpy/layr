#!/bin/bash
# ci: overlayfs
# Reboot on OverlayFS: a layer taken from a saved cache state is mounted again by the next command,
# with its files and its taken flag, also after its source line is deleted (the state's owner keeps
# it). A layer that cannot be mounted again fails the command with a clear message.
# The btrfs CI job runs every test: this one needs an OverlayFS run (the registry, its mounts).
if [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    echo "t117-overlayfs-reboot.sh: 0 passed, 0 failed (not an OverlayFS run)"
    exit 0
fi
. "$(dirname "$0")/lib.sh"

# The registry entry of a path: "taken" flag and the lower object id, from the store's registry.
cat > "$T/entry.py" <<'PY'
import json, sys
reg = json.load(open(sys.argv[1]))
hits = [e for p, e in reg["trees"].items() if p.endswith(sys.argv[2])]
if not hits:
    print("missing")
elif sys.argv[3] == "taken":
    print(str(hits[0].get("taken", False)))
else:
    print(hits[0]["base"])
PY
REG="$LAYR_ROOT/.layr-overlayfs/registry.json"
entry() { python3 -I "$T/entry.py" "$REG" "$1" "$2"; }

sample "$T/src"
l init rb --from "$T/src" -q >/dev/null
R=$LAYR_ROOT/projects/rb/lines
W=$R/w
cd "$R/main" || exit 1
l config regenerable '["target"]'
mkdir -p target
head -c 5000000 /dev/urandom > target/blob
sum=$(cksum < target/blob)
l save -q
l line new -q s main
l line new -q w s
eq "the new line has the cache of its source" "$sum" "$(cksum < "$W/target/blob")"
eq "its cache layer is taken" "True" "$(entry /lines/w/target taken)"

# A reboot: every mount of the line goes; the registry stays.
reboot() {
    umount -R "$W" 2>/dev/null || true
    eq "the line is unmounted" 0 "$(awk -v p="$W" '$5 == p || index($5, p "/") == 1' /proc/self/mountinfo | wc -l)"
}
reboot
cd "$W" || exit 1
ok "the next command runs after the reboot" l status -s
ok "the cache layer is mounted again" mountpoint -q "$W/target"
eq "target has its files after the reboot" "$sum" "$(cksum < "$W/target/blob")"
eq "the layer is still taken" "True" "$(entry /lines/w/target taken)"

# Delete the source line: its cache state stays with the layer that took it.
cd "$R/main" || exit 1
l line delete -D s >/dev/null
reboot
cd "$W" || exit 1
ok "the command runs after the source line is gone" l status -s
eq "target still has its files" "$sum" "$(cksum < "$W/target/blob")"
eq "the layer is still taken" "True" "$(entry /lines/w/target taken)"

# A layer that cannot be mounted again: the command fails and names the mount.
lower=$(entry /lines/w/target base)
reboot
rm -rf "$LAYR_ROOT/.layr-overlayfs/objects/$lower"
cd "$W" || exit 1
out=$(l status -s 2>&1)
rc=$?
eq "the command fails when the layer cannot be mounted" 1 "$([ "$rc" -ne 0 ] && echo 1 || echo 0)"
has "the error names the mount" "mount OverlayFS" "$out"
finish
