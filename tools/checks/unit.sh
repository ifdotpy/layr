#!/bin/bash
set -euo pipefail
. tools/checks/environment.sh
export CARGO_BUILD_JOBS=1
cargo test --locked --offline --workspace
