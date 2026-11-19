#!/bin/bash
# ci: macos overlayfs
# Named warnings run once; automatic object writes reuse their checked manifest.
. "$(dirname "$0")/lib.sh"
TEST_DIR=$(cd "$(dirname "$0")" && pwd)
python3 "$TEST_DIR/save-reads.py" "$T" "$LAYR"
ok "save comparison reads and complete object manifest" test "$?" = 0
finish
