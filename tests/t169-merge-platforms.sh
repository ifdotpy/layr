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
finish
