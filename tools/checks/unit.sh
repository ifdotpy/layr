#!/bin/bash
set -euo pipefail
. tools/checks/environment.sh
python3 tools/checks/test_run.py
python3 tests/test_run_harness.py
# The unit tests of ci.yml's test job: layr-retest's end-to-end tests need nextest and LLVM tools.
cargo test --locked --offline --workspace --exclude layr-retest
cargo test --locked --offline -p layr-retest --lib --bins
