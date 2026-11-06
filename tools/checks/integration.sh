#!/bin/bash
# Linux btrfs counterpart of .github/workflows/ci.yml.
set -euo pipefail
. tools/checks/environment.sh
test "$(id -u)" = 0
export LAYR_MINIO_BIN=/usr/local/lib/layr-checks/minio
test -x "$LAYR_MINIO_BIN"
command -v helm
modprobe btrfs || true
# Automatic checks run in bubblewrap as the caller; Ubuntu 24.04 blocks the user namespaces it needs.
sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 2>/dev/null || true
cargo build --locked --offline
cargo test --locked --offline
export LAYR="$PWD/target/debug/layr"
bash tests/run.sh
