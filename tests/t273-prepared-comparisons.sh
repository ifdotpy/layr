#!/bin/bash
# ci: macos overlayfs
# Save and FF merge reuse comparisons and reject concurrent writes.
. "$(dirname "$0")/lib.sh"
TEST_DIR=$(cd "$(dirname "$0")" && pwd)
python3 "$TEST_DIR/prepared-comparisons.py" "$LAYR" "$T"
ok "owned prepared comparisons" test "$?" = 0
finish
