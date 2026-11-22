#!/bin/bash
# Three rounds of bench.sh on both trees; the load average before each run goes to results/load.txt.
# Needs the variables of bench.sh. usage: run3.sh [backends]
set -euo pipefail
: "${B:?}"
here=$(cd "$(dirname "$0")" && pwd)
# The user's index server (`layr tool` on the native backends) gets its own socket folder: a
# detached run outlives the login session, and logind removes /run/user/<uid> with it.
mkdir -p -m 0700 "$B/fs"
export LAYR_FS_SOCKET=$B/fs/layrfs.sock
for round in ${ROUNDS:-1 2 3}; do
    for t in "layr /home/layrfs-src/layr build" "linux /home/layrfs-src/linux"; do
        set -- $t
        echo "round $round $1: $(uptime)" | tee -a "$B/results/load.txt"
        ps -eo pcpu,comm --sort=-pcpu | sed -n 2,6p | tr '\n' ';' | sed 's/^/  busiest: /' >> "$B/results/load.txt"; echo >> "$B/results/load.txt"
        ROUND=$round bash "$here/bench.sh" "$@"
    done
done
echo ALLDONE
