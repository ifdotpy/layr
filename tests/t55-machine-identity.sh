#!/bin/bash
# ci: macos-plain
. "$(dirname "$0")/lib.sh"
python3 "$(dirname "$0")/machine-identity.py" "$LAYR" "$T"
