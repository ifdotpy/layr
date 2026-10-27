#!/bin/bash
. "$(dirname "$0")/lib.sh"
python3 "$(dirname "$0")/git-init-space.py" "$LAYR" "$T"
