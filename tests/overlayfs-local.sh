#!/bin/bash
# Run only in an isolated Linux VM or container with mount permission.
set -euo pipefail
: "${LAYR_OVERLAYFS_TEST_ROOT:?Set an isolated test directory}"
LAYR=${LAYR:-$PWD/target/debug/layr}
T=$(mktemp -d "$LAYR_OVERLAYFS_TEST_ROOT/overlay-local.XXXXXX")
export LAYR_ROOT=$T/store LAYR_AUTHOR_NAME=Test LAYR_AUTHOR_EMAIL=test@example.invalid
mkdir -p "$T/source/dir"
printf base > "$T/source/file"
printf nested > "$T/source/dir/file"
ln -s file "$T/source/link"
"$LAYR" init check --from "$T/source" -q
cd "$LAYR_ROOT/projects/check/lines/main"
"$LAYR" branch -q topic
cd ../topic
printf changed > file
rm dir/file
"$LAYR" add -A
"$LAYR" commit -qm changed
[ "$(cat ../main/file)" = base ]
[ "$(cat ../main/dir/file)" = nested ]
"$LAYR" branch -m topic renamed
cd ../renamed
[ "$(cat file)" = changed ]
[ ! -e dir/file ]
"$LAYR" fsck
# Simulate a VM restart by removing only this store's mounts.
cd /
findmnt -rn -o TARGET | awk -v p="$LAYR_ROOT/" 'index($0,p)==1' | sort -r | while read -r m; do umount "$m"; done
"$LAYR" --root "$LAYR_ROOT" --project check --line renamed log -1
cd "$LAYR_ROOT/projects/check/lines/renamed"
[ "$(cat file)" = changed ]
[ ! -e dir/file ]
"$LAYR" fsck
printf 'PASS OverlayFS writes, whiteouts, rename, and mount recovery\n'
