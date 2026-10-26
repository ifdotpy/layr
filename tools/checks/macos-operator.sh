#!/bin/bash
# One-time root verification, outside automatic checks. Arguments: binary, existing Mac username.
set -euo pipefail
test "$(uname -s)" = Darwin
if [ "${1:-}" = --keychain ]; then
    test "$(id -u)" != 0
    cd "$(dirname "$0")/../.."
    for name in disk::catalog::tests::status_reports_the_keychain_and_service_without_the_identity \
        disk::keystore::tests::login_keychain_roundtrip \
        disk::tests::keychain_lock_hides_files_and_blocks_webdav_until_unlock; do
        cargo test --locked --bin layr "$name" -- --exact
    done
    exit
fi
test "$(id -u)" = 0
binary=${1:?usage: sudo bash tools/checks/macos-operator.sh /absolute/path/to/layr username}
runner_user=${2:?existing non-root Mac username required}
cd "$(dirname "$0")/../.."
test "$(id -u "$runner_user")" != 0
diskutil enableOwnership /Volumes/LayrRunner
diskutil enableOwnership /Volumes/LayrRunnerCase
env LAYR="$binary" LAYR_APFS_TEST_ROOT=/Volumes/LayrRunner \
    LAYR_APFS_TEST_USER="$runner_user" bash tests/apfs-service.sh
