#!/bin/bash
# Run on the project creator's machine. Runner copies consume this signed policy.
set -euo pipefail
layr config check.unit 'bash tools/checks/unit.sh'
layr config check.integration 'bash tools/checks/integration.sh'
layr config check.overlayfs-ext4 'bash tools/checks/overlayfs.sh ext4'
layr config check.overlayfs-xfs 'bash tools/checks/overlayfs.sh xfs'
layr config check.apfs 'bash tools/checks/apfs.sh'
layr config checks.changes '["unit","integration","overlayfs-ext4","overlayfs-xfs","apfs"]'
layr config checks.lines '{"main":["unit","integration","overlayfs-ext4","overlayfs-xfs","apfs"]}'
layr config checks.platforms '{"unit":"linux","integration":"linux","overlayfs-ext4":"linux","overlayfs-xfs":"linux","apfs":"macos"}'
layr config checks.privileged '["integration","overlayfs-ext4","overlayfs-xfs"]'
layr config checks.timeout 7200
layr config checks.memory 8589934592
# RLIMIT_NPROC counts every process of the Mac desktop user; Linux also has TasksMax.
layr config checks.processes 4096
layr config checks.log-bytes 16777216
