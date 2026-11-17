#!/bin/bash
# ci: macos
# Not on OverlayFS yet: the saved status of a mounted line is not reused there (known gap of the fs stack).
# Shared status validation on object-backed history, including persistent mmap writes.
. "$(dirname "$0")/lib.sh"
TEST_DIR=$(cd "$(dirname "$0")" && pwd)
python3 "$TEST_DIR/status-stat-cache.py" "$LAYR" "$T"
ok "shared status cache regression" test "$?" = 0
finish
