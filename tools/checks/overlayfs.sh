#!/bin/bash
# Linux ext4/xfs counterparts of .github/workflows/overlayfs.yml.
set -euo pipefail
. tools/checks/environment.sh
. tools/checks/steps.sh
test "$(id -u)" = 0
filesystem=${1:?usage: overlayfs.sh ext4|xfs}
case "$filesystem" in ext4|xfs) ;; *) exit 2 ;; esac
image="$TMPDIR/overlayfs.img"
root="$TMPDIR/overlayfs"
fold="$TMPDIR/fold"
cleanup() {
    mountpoint -q "$fold" && umount -R "$fold" || true
    mountpoint -q "$root" && umount -R "$root" || true
}
trap cleanup EXIT
truncate -s 8G "$image"
if [ "$filesystem" = ext4 ]; then mkfs.ext4 -q -F "$image"; else mkfs.xfs -q -f "$image"; fi
mkdir "$root"
mount -o loop "$image" "$root"
mount --make-private "$root"
cargo build --locked --offline --features overlayfs
export LAYR="$PWD/target/debug/layr" LAYR_OVERLAYFS_TEST_ROOT="$root"
cargo test --locked --offline --features overlayfs --no-run --message-format=json > "$TMPDIR/unit.json"
python3 - "$TMPDIR/unit.json" > "$TMPDIR/unit-paths" <<'PY'
import json,sys
for line in open(sys.argv[1]):
    item=json.loads(line)
    if item.get('executable') and item.get('profile',{}).get('test'):
        print(item['executable'])
PY
while IFS= read -r unit; do step "unit tests $(basename "$unit")" env TMPDIR="$root" "$unit" --include-ignored; done < "$TMPDIR/unit-paths"
step overlayfs-local bash tests/overlayfs-local.sh
step overlayfs-caches bash tests/overlayfs-caches.sh
step save-unsettled bash tests/save-unsettled.sh
casefold_refused() {
    if env LAYR_ROOT="$fold/f/store" LAYR_AUTHOR_NAME=T LAYR_AUTHOR_EMAIL=t@x.invalid \
        "$LAYR" init p --from "$TMPDIR/fold-src" -q 2> "$TMPDIR/fold.err"; then
        echo 'casefold store was accepted' >&2; return 1
    fi
    grep 'ignores case' "$TMPDIR/fold.err"
}
if [ "$filesystem" = ext4 ]; then
    truncate -s 64M "$TMPDIR/fold.img"
    mkfs.ext4 -q -F -O casefold "$TMPDIR/fold.img"
    mkdir "$fold" "$TMPDIR/fold-src"
    mount -o loop "$TMPDIR/fold.img" "$fold"
    mount --make-private "$fold"
    mkdir "$fold/f"
    chattr +F "$fold/f"
    echo x > "$TMPDIR/fold-src/a"
    step "casefold store refused" casefold_refused
    umount "$fold"
fi
step "shell tests (overlayfs)" bash tests/run.sh --tag overlayfs
finish_steps
