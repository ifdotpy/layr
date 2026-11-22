#!/bin/bash
# Common helpers of the LayrFS lab experiments (kukuka-wsl).
set -euo pipefail
export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH
B=$HOME/layrfs-bench
LF=$B/src/lab/tools/layrfs-fuse/target/release/layrfs-fuse
L=/home/layr/layr-lab
O=/home/layr/store/objects
me=$(id -u)
R=$HOME/lfx/lab/results.jsonl
now() { date +%s%N; }
ms() { echo $(( ($2 - $1) / 1000000 )); }
rec() { echo "{\"exp\":\"$1\",\"step\":\"$2\",\"ms\":$3${4:+,$4}}" >> "$R"; printf '%-4s %-44s %8s ms %s\n' "$1" "$2" "$3" "${4:-}"; }
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
vnew() { # vnew <name> <state> [extra options]; OBJ overrides the store
    local n=$1 st=$2; shift 2
    mkdir -p "$B/caches/$n/target"
    sudo -u layr $L fs view start --objects "${OBJ:-$O}" --state "$st" --line /home/layr/store/lines/$n --mnt /home/layr/lines/$n \
        --sock /home/layr/run/$n.sock --cache target=$B/caches/$n/target --allow-other --allow $me --owner $me ${EXTRA:---no-open --uring} "$@"
}
vctl() { local n=$1; shift; $L fs view ctl /home/layr/run/$n.sock "$@"; }
vdel() { vctl "$1" stop remove >/dev/null || true; rm -rf "$B/caches/$1"; }
M() { echo /home/layr/lines/$1; }
LIST=$B/tmp/linux.files
LLIST=$B/tmp/layr.files
