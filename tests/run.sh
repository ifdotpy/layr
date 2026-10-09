#!/bin/bash
# Run the integration tests. Needs root, btrfs-progs, git, zstd, python3 and loop devices.
# Usage: sudo tests/run.sh [t01-basic ...]
set -u
cd "$(dirname "$0")/.." || exit 1
export LAYR=${LAYR:-$PWD/target/debug/layr}
files=("$@")
[ ${#files[@]} -eq 0 ] && files=($(ls tests/t*.sh | xargs -n1 basename | sed 's/\.sh$//'))
failed=0
for t in "${files[@]}"; do
    out=$(bash "tests/$t.sh" 2>&1)
    echo "$out" | grep -E '^(FAIL|  )' || true
    summary=$(echo "$out" | tail -1)
    echo "$summary"
    case "$summary" in *", 0 failed") ;; *) failed=1 ;; esac
done
exit $failed
