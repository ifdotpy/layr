#!/bin/bash
# ci: macos overlayfs
. "$(dirname "$0")/lib.sh"
python3 "$(dirname "$0")/op-log-limit.py" "$LAYR" "$T"
