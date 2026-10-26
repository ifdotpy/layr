#!/bin/bash
# Unprivileged counterpart of ci.yml's macos job. Volumes are prepared once by the operator.
set -euo pipefail
test "$(uname -s)" = Darwin
test "$(id -u)" != 0
. tools/checks/environment.sh
export LAYR_APFS_TEST_ROOT=/Volumes/LayrRunner
case_root=/Volumes/LayrRunnerCase
for volume in "$LAYR_APFS_TEST_ROOT" "$case_root"; do
    diskutil info -plist "$volume" | python3 -c '
import plistlib,sys
p=plistlib.loads(sys.stdin.buffer.read())
assert p.get("FilesystemType")=="apfs" and p.get("MountPoint")==sys.argv[1], p
' "$volume"
    test -w "$volume"
done
cargo build --locked --offline
cargo test --locked --offline -- --include-ignored
export LAYR="$PWD/target/debug/layr"
python3 tests/apfs-index.py --binary "$LAYR" --root "$LAYR_APFS_TEST_ROOT"
python3 tests/apfs-thin.py --binary "$LAYR" --root "$LAYR_APFS_TEST_ROOT"
python3 tests/apfs-status-cache.py --binary "$LAYR" --root "$LAYR_APFS_TEST_ROOT"
mkdir -p "$TMPDIR/case"
python3 tests/apfs-case.py --binary "$LAYR" --root "$TMPDIR/case"
bash tests/apfs-local.sh
LAYR_TEST_DIRECT_MAIN=1 bash tests/run.sh t01-basic t02-merge t03-git t05-transfer t06-sync t08-gitcompat
LAYR_APFS_TEST_ROOT="$case_root" LAYR_TEST_DIRECT_MAIN=1 bash tests/run.sh t01-basic t02-merge t03-git
LAYR_APFS_TEST_ROOT="$case_root" python3 tests/apfs-thin.py --binary "$LAYR" --root "$case_root"
