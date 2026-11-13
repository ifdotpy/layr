#!/bin/bash
. "$(dirname "$0")/lib.sh"
# cli-help.py prints the result line; a failed assertion prints a traceback only, so give the result line here.
python3 "$(dirname "$0")/cli-help.py" "$LAYR" "$T" || { echo "$(basename "$0"): 0 passed, 1 failed"; exit 1; }
