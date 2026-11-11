#!/bin/bash
# ci: macos
# A required check with platforms (`checks.platforms`) needs a passing run on each of them before a merge moves the
# line. This machine runs the check of its own platform; a platform it is not keeps a queue entry pending (the entry
# waits for that runner), and a direct merge fails with the reason. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
here=$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/macos/')-$(uname -m | sed 's/arm64/aarch64/')
other=linux-x86_64; [ "$here" = linux-x86_64 ] && other=linux-aarch64
l protect main --check gate >/dev/null
l config check.gate "echo gate" >/dev/null
l config checks.platforms "{\"gate\":[\"$here\",\"$other\"]}" >/dev/null
BASE=$(l rev-parse head)
l line new -q topic
cd ../topic || exit 1
echo one > one.txt; l add one.txt; l save -qm one
l queue add topic >/dev/null
cd ../main || exit 1
out=$(l queue run --into main 2>&1)
has "the entry waits for the runner of the other platform" "has no passing run on $other" "$out"
eq "main stays where it was" "$BASE" "$(l rev-parse head)"
eq "the entry is still pending" pending "$(l queue list --into main --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
l config checks.platforms "{\"gate\":\"$here\"}" >/dev/null
ok "with only this platform the entry merges" l queue run --into main
eq "the entry is done" done "$(l queue list --into main --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"

# A failed run on the other platform fails the entry with the reason: no runner runs that exact check again, so the
# entry would otherwise stay pending for good. The run is this machine's, recorded as the other platform.
if in_check_sandbox && ! /usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true 2>/dev/null; then
    echo "(the failed-elsewhere case runs a nested check sandbox: skipped here)"
else
    l config check.gate "exit 3" >/dev/null
    l config checks.platforms "{\"gate\":\"$other\"}" >/dev/null
    l config checks.changes '["gate"]' >/dev/null
    l config --local checks.runner true >/dev/null
    HEAD=$(l rev-parse head)
    cd ../topic || exit 1
    echo two > two.txt; l add two.txt; l save -qm two; S=$(l rev-parse head); l approve head -q
    l queue add topic >/dev/null
    LAYR_TEST_PLATFORM=$other l checks run --state "$S" --json >/dev/null 2>&1
    cd ../main || exit 1
    rc=0; out=$(l queue run --into main 2>&1) || rc=$?
    eq "a failed run on the other platform fails the queue run" 1 "$rc"
    has "the reason names the check, its exit and the platform" "check 'gate' failed (exit 3) on $other" "$out"
    eq "the entry is failed, not pending" failed "$(l queue list --into main --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[-1]["status"])')"
    eq "main stays where it was" "$HEAD" "$(l rev-parse head)"
fi
finish
