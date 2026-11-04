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
# A test that runs longer than LAYR_TEST_TIMEOUT seconds (default 900) is stopped: its output so
# far and the process list are printed, its process group is killed, and it counts as failed.
timeout_s=${LAYR_TEST_TIMEOUT:-900}

# descendants <pid>: the pids below <pid> (children first), from the process table.
descendants() {
    local p=$1 c
    for c in $(ps -axo pid=,ppid= | awk -v p="$p" '$2 == p { print $1 }'); do
        descendants "$c"
        echo "$c"
    done
}

# run_test <name> <log>: runs the test in its own process group. Returns 124 on timeout.
run_test() {
    local t=$1 log=$2 pid waited=0 victims
    set -m
    bash "tests/$t.sh" > "$log" 2>&1 &
    pid=$!
    set +m
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$timeout_s" ]; then
            victims=$( (descendants "$pid"; echo "$pid") | tr '\n' ' ')
            # The process list keeps the test's processes and every layr process (the whole table
            # is too long for a log).
            local pids
            pids=$(echo $victims | tr ' ' '|')
            {
                echo "TIMEOUT $t: still running after $timeout_s s; stopping it"
                echo "process list (the test's processes and layr processes):"
                if [ "$(uname -s)" = Linux ]; then ps -ef --forest; else ps -axo pid,ppid,stat,command; fi |
                    awk -v pids="$pids" 'NR == 1 || /\/layr( |$)|\/layr-|tests\/(run\.sh|t[0-9])/ || $0 ~ ("(^| )(" pids ")( |$)")'
            } >> "$log" 2>&1
            kill -TERM -- "-$pid" 2>/dev/null
            for v in $victims; do kill -TERM "$v" 2>/dev/null; done
            sleep 5
            kill -KILL -- "-$pid" 2>/dev/null
            for v in $victims; do kill -KILL "$v" 2>/dev/null; done
            wait "$pid" 2>/dev/null
            return 124
        fi
        sleep 1
        waited=$((waited + 1))
    done
    wait "$pid"
}

failed=0
logdir=$(mktemp -d "${TMPDIR:-/tmp}/layr-run.XXXXXX")
for t in "${files[@]}"; do
    log="$logdir/$t.log"
    rc=0
    run_test "$t" "$log" || rc=$?
    out=$(cat "$log")
    summary=$(echo "$out" | tail -1)
    if [ "$rc" -eq 124 ]; then
        printf '%s\n' "$out"
        echo "FAIL $t: timeout after $timeout_s s"
        failed=1
        continue
    fi
    # TEMPORARY (#213): the free space after each test, to find the test that fills the runner.
    echo "  free: $(df -Pk "${TMPDIR:-/var/tmp}" | awk 'NR==2 {print int($4/1024)}') MiB after $t"
    case "$summary" in
        *", 0 failed") echo "$summary" ;;
        *) printf '%s\n' "$out"; failed=1 ;;
    esac
done
rm -rf "$logdir"
exit $failed
