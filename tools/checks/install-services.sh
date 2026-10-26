#!/bin/bash
# Run as root after provisioning the dedicated runner's own store and project role.
set -euo pipefail
project=${1:?usage: install-services.sh project}
test "$(id -u)" = 0
for unit in tools/checks/*.service tools/checks/*.timer; do install -m 644 "$unit" /etc/systemd/system/; done
systemctl daemon-reload
systemctl enable --now layr-runner-daemon.service
systemctl enable --now "layr-checks@$project.service" "layr-sync@$project.timer"
