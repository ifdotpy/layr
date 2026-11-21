#!/bin/bash
# ci: macos overlayfs
# Issues #440/#441: a save must never retain the old tail at an exact byte boundary.
. "$(dirname "$0")/lib.sh"
python3 "$(dirname "$0")/save-shrink-boundary.py" "$T" "$LAYR"
eq "shrinking named, staged and automatic saves preserve every byte" 0 "$?"
finish
