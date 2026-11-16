#!/bin/bash
# ci: macos-plain
# serial
# It measures the free space of the volume (git-init-space.py), which tests that run at the same
# time change: with 8 at once, init "allocated" 26 MB for an unchanged 128 MiB file (2026-11-14).
. "$(dirname "$0")/lib.sh"
python3 "$(dirname "$0")/git-init-space.py" "$LAYR" "$T"
