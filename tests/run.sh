#!/bin/bash
# Run the integration tests. Needs root, btrfs-progs, git, zstd, python3 and loop devices.
# Usage: sudo tests/run.sh [t01-basic ...]
#        tests/run.sh --tag <tag>: the tests whose "# ci:" line (line 2) names <tag>. CI selects its
#        tests this way, so a new test changes only its own file (no shared list to conflict on).
set -u
cd "$(dirname "$0")/.." || exit 1
export LAYR=${LAYR:-$PWD/target/debug/layr}
files=("$@")
if [ "${1:-}" = --tag ]; then
    [ $# -eq 2 ] || { echo "usage: tests/run.sh --tag <tag>" >&2; exit 2; }
    files=($(grep -l -E "^# ci:(.* )?$2( |\$)" tests/t*.sh | xargs -n1 basename | sed 's/\.sh$//'))
    [ ${#files[@]} -gt 0 ] || { echo "no test has the tag $2" >&2; exit 2; }
fi
[ ${#files[@]} -eq 0 ] && files=($(ls tests/t*.sh | xargs -n1 basename | sed 's/\.sh$//'))
failed=0
for t in "${files[@]}"; do
    out=$(bash "tests/$t.sh" 2>&1)
    summary=$(echo "$out" | tail -1)
    case "$summary" in
        *", 0 failed") echo "$summary" ;;
        *) printf '%s\n' "$out"; failed=1 ;;
    esac
done
exit $failed
