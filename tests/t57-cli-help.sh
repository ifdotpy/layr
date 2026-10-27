#!/bin/bash
. "$(dirname "$0")/lib.sh"
python3 "$(dirname "$0")/cli-help.py" "$LAYR" "$T"
