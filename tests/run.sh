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
# LAYR_TEST_JOBS tests run at once (default 1). Each has its own folder (lib.sh), and the output
# keeps the order of the list. More than one needs bash 4.3 (wait -n).
jobs_n=${LAYR_TEST_JOBS:-1}
[[ "$jobs_n" =~ ^[1-9][0-9]*$ ]] && [ "${#jobs_n}" -le 6 ] || {
    echo "LAYR_TEST_JOBS must be a positive integer with at most six digits" >&2
    exit 2
}
if [ "$jobs_n" -gt 1 ] && { [ "${BASH_VERSINFO[0]}" -lt 4 ] ||
    { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 3 ]; }; }; then
    echo "parallel tests require Bash 4.3 or later" >&2
    exit 2
fi

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
failed_tests=()
logdir=$(mktemp -d "${TMPDIR:-/tmp}/layr-run.XXXXXX")
# report <name>: print a finished test's summary, or its whole output when it failed.
report() {
    local t=$1 rc seconds out summary
    read -r rc seconds < "$logdir/$t.rc"
    out=$(cat "$logdir/$t.log")
    summary=$(echo "$out" | tail -1)
    if [ "$rc" -eq 124 ]; then
        printf '%s\n' "$out"
        echo "FAIL $t: timeout after $timeout_s s"
        failed=1; failed_tests+=("$t")
        return
    fi
    if [ "$rc" -ne 0 ]; then
        printf '%s\n' "$out"
        echo "FAIL $t: exit $rc"
        failed=1; failed_tests+=("$t")
        return
    fi
    case "$summary" in
        *", 0 failed")
            # Add timing only after the original summary passes its exact check.
            [ -n "${LAYR_TEST_TIMES:-}" ] && summary="$summary (${seconds} s)"
            echo "$summary" ;;
        *) printf '%s\n' "$out"; failed=1; failed_tests+=("$t") ;;
    esac
}
one() {
    local rc=0 started=$SECONDS
    run_test "$1" "$logdir/$1.log" || rc=$?
    echo "$rc $((SECONDS - started))" > "$logdir/$1.rc.tmp" && mv "$logdir/$1.rc.tmp" "$logdir/$1.rc"
}
# A test whose line 3 or later says `# serial` measures time: it runs alone, after the others.
serial=()
if [ "$jobs_n" -gt 1 ]; then
    parallel=()
    for t in "${files[@]}"; do
        if grep -qx '# serial' "tests/$t.sh"; then serial+=("$t"); else parallel+=("$t"); fi
    done
    # The longest first (tests/durations.txt; a test that is not there counts as 10 s): in list order, the two
    # slowest macOS tests (226 and 181 s) started near the end, and the step took 684 s for 4042 s of tests on
    # 8 jobs (2026-11-22). Equal times keep the list order.
    if [ "${#parallel[@]}" -gt 1 ]; then
        parallel=($(printf '%s\n' "${parallel[@]}" | awk -v durations=tests/durations.txt '
            BEGIN {
                while ((getline < durations) > 0)
                    if (!($2 in seconds)) seconds[$2] = $1
                close(durations)
            }
            { print ($0 in seconds ? seconds[$0] : 10), NR, $0 }
        ' | sort -k1,1rn -k2,2n | awk '{ print $3 }'))
    fi
    files=("${parallel[@]}" "${serial[@]}")
fi
next=0
flush() {
    while [ "$next" -lt "${#files[@]}" ] && [ -f "$logdir/${files[$next]}.rc" ]; do
        report "${files[$next]}"
        next=$((next + 1))
    done
}
for t in "${files[@]}"; do
    if [ "$jobs_n" -le 1 ] || [[ " ${serial[*]} " == *" $t "* ]]; then
        wait; flush
        one "$t"
    else
        while [ "$(jobs -rp | wc -l)" -ge "$jobs_n" ]; do wait -n; flush; done
        one "$t" &
    fi
    flush
done
while [ "$next" -lt "${#files[@]}" ] && [ "$(jobs -rp | wc -l)" -gt 0 ]; do
    wait -n || true
    flush
done
wait
flush
# A failed test runs once more, alone (LAYR_TEST_RETRY=0 turns this off). One that passes then is flaky: it is
# named (FLAKY) and does not fail the run, so a timing flake does not block a merge; fix it separately.
if [ "${LAYR_TEST_RETRY:-1}" = 1 ] && [ "${#failed_tests[@]}" -gt 0 ] && [ "${#failed_tests[@]}" -le 5 ]; then
    still=()
    for t in "${failed_tests[@]}"; do
        rc=0
        run_test "$t" "$logdir/$t.retry.log" || rc=$?
        if [ "$rc" -eq 0 ] && tail -1 "$logdir/$t.retry.log" | grep -q ', 0 failed$'; then
            echo "FLAKY $t: failed, then passed when run again alone"
        else
            echo "FAIL $t: failed again when run alone"
            still+=("$t")
        fi
    done
    [ "${#still[@]}" -eq 0 ] && failed=0
fi
# LAYR_TEST_TIMES: the slowest tests last, so a long run shows where its time went.
if [ -n "${LAYR_TEST_TIMES:-}" ]; then
    echo "SLOWEST TESTS (seconds):"
    for t in "${files[@]}"; do
        [ -f "$logdir/$t.rc" ] && read -r _ seconds < "$logdir/$t.rc" && echo "$seconds $t"
    done | sort -rn | head -15 | sed 's/^/  /'
fi
if [ "$next" -ne "${#files[@]}" ]; then
    echo "FAIL runner: a test returned no result"
    failed=1
fi
rm -rf "$logdir"
exit $failed
