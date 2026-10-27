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
cargo test --locked --offline -- --include-ignored \
    --skip store_remote::backend::tests::reader_hooks_use_s3_caller_credentials_and_leave_file_reads_local \
    --skip store_remote::gateway::tests::epoch_slot_staging_uses_the_project_grant_before_a_new_proof_exists \
    --skip store_remote::gateway::tests::missing_descriptor_keeps_optional_person_reads_and_hints_machine_grants \
    --skip cmd::checks::macos_tests::sandbox_refuses_network_and_writes_outside_work \
    --skip disk::catalog::tests::status_reports_the_keychain_and_service_without_the_identity \
    --skip disk::keystore::tests::login_keychain_roundtrip \
    --skip disk::tests::keychain_lock_hides_files_and_blocks_webdav_until_unlock
export LAYR="$PWD/target/debug/layr"
python3 tests/apfs-index.py --binary "$LAYR" --root "$LAYR_APFS_TEST_ROOT"
python3 tests/apfs-thin.py --binary "$LAYR" --root "$LAYR_APFS_TEST_ROOT"
python3 tests/apfs-status-cache.py --binary "$LAYR" --root "$LAYR_APFS_TEST_ROOT"
mkdir -p "$TMPDIR/case"
python3 tests/apfs-case.py --binary "$LAYR" --root "$TMPDIR/case" \
    --case-sensitive-root "$LAYR_APFS_TEST_ROOT" --case-insensitive-root "$case_root"
bash tests/apfs-local.sh
LAYR_TEST_DIRECT_MAIN=1 bash tests/run.sh t01-basic t02-merge t03-git t05-transfer t06-sync t08-gitcompat
LAYR_APFS_TEST_ROOT="$case_root" LAYR_TEST_DIRECT_MAIN=1 bash tests/run.sh t01-basic t02-merge t03-git
LAYR_APFS_TEST_ROOT="$case_root" python3 tests/apfs-thin.py --binary "$LAYR" --root "$case_root"
