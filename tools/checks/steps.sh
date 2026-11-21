# Test steps of a check that run to the end: a failed step is named and the next one runs, so one run of a
# check shows every failure, not only the first (a release took 10 queue runs for that, 2026-11-17). Build
# and setup steps stay outside `step` and still stop the check at once. Sourced after environment.sh.
failed_steps=()
# step <name> <command...>: run one test step; a failure is recorded, not fatal.
# Each step prints its time ("STEP TIME"): a check of 20 minutes showed no part of where the time went
# (2026-11-21). finish_steps lists them again, slowest first.
step_times=()
step() {
    local name=$1 rc=0 start=$SECONDS
    shift
    "$@" || rc=$?
    local took=$((SECONDS - start))
    echo "STEP TIME: $name ${took}s (exit $rc)"
    step_times+=("$took $name")
    if [ "$rc" -ne 0 ]; then
        echo "STEP FAILED: $name (exit $rc)"
        failed_steps+=("$name")
    fi
}
# finish_steps: the summary; exit 1 when a step failed.
finish_steps() {
    if [ "${#step_times[@]}" -gt 0 ]; then
        echo "STEP TIMES (seconds, slowest first):"
        printf '%s\n' "${step_times[@]}" | sort -rn | sed 's/^/  /'
    fi
    if [ "${#failed_steps[@]}" -gt 0 ]; then
        echo "FAILED STEPS (${#failed_steps[@]}):"
        printf '  %s\n' "${failed_steps[@]}"
        exit 1
    fi
    echo "all steps passed"
}
