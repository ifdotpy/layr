#!/bin/bash
# ci: macos
# Not on OverlayFS yet: the prepared comparisons of a mounted line are not reused there (known gap of the fs stack).
# Save and FF merge reuse comparisons and reject concurrent writes.
. "$(dirname "$0")/lib.sh"
TEST_DIR=$(cd "$(dirname "$0")" && pwd)
python3 "$TEST_DIR/prepared-comparisons.py" "$LAYR" "$T"
ok "owned prepared comparisons" test "$?" = 0
finish
