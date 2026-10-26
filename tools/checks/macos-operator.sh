#!/bin/bash
# One-time root verification, outside automatic checks. Arguments: binary, existing Mac username.
set -euo pipefail
test "$(uname -s)" = Darwin
test "$(id -u)" = 0
binary=${1:?usage: sudo bash tools/checks/macos-operator.sh /absolute/path/to/layr username}
runner_user=${2:?existing non-root Mac username required}
cd "$(dirname "$0")/../.."
test "$(id -u "$runner_user")" != 0
diskutil enableOwnership /Volumes/LayrRunner
diskutil enableOwnership /Volumes/LayrRunnerCase
env LAYR="$binary" LAYR_APFS_TEST_ROOT=/Volumes/LayrRunner \
    LAYR_APFS_TEST_USER="$runner_user" bash tests/apfs-service.sh
