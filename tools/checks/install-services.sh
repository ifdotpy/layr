#!/bin/bash
# Run as root after provisioning the dedicated runner's own store and project role.
# With CHECKS (a comma list, such as unit), the runner takes only those checks of queued and recent
# states (layr-scoped-checks@PROJECT, scoped-watch.py) in place of every pending check.
set -euo pipefail
project=${1:?usage: install-services.sh project [checks]} checks=${2:-}
test "$(id -u)" = 0
for unit in tools/checks/*.service tools/checks/*.timer; do install -m 644 "$unit" /etc/systemd/system/; done
install -D -m 755 tools/checks/scoped-watch.py /usr/local/lib/layr-runner/scoped-watch.py
systemctl daemon-reload
watcher="layr-checks@$project.service"
if [ -n "$checks" ]; then
    install -d -m 755 "/etc/systemd/system/layr-scoped-checks@$project.service.d"
    printf '[Service]\nEnvironment=LAYR_CHECK_SCOPE=%s\n' "$checks" \
        > "/etc/systemd/system/layr-scoped-checks@$project.service.d/scope.conf"
    systemctl disable --now "$watcher"
    watcher="layr-scoped-checks@$project.service"
    systemctl daemon-reload
fi
systemctl enable --now layr-runner-daemon.service
systemctl enable --now "$watcher" "layr-sync@$project.timer"
