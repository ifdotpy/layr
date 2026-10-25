#!/bin/bash
# Run as a project maintainer, in the layr project. Commands remain existing check.<name> keys.
set -euo pipefail
layr config check.unit 'bash tools/checks/unit.sh'
layr config check.integration 'bash tools/checks/integration.sh'
layr config check.overlayfs-ext4 'bash tools/checks/overlayfs.sh ext4'
layr config check.overlayfs-xfs 'bash tools/checks/overlayfs.sh xfs'
layr config checks.changes '["unit","integration","overlayfs-ext4","overlayfs-xfs"]'
layr config checks.lines '{"main":["unit","integration","overlayfs-ext4","overlayfs-xfs"]}'
layr config checks.privileged '["integration","overlayfs-ext4","overlayfs-xfs"]'
layr config checks.timeout 7200
layr config checks.memory 8589934592
layr config checks.processes 512
layr config checks.log-bytes 16777216
