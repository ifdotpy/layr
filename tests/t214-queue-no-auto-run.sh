#!/bin/bash
# ci: macos overlayfs
# Adding an entry for a guarded runner must not run the configured local queue.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l config queue.run main
l line new -q topic main
base=$(l rev-parse main)
cd ../topic || exit 1
echo topic > topic.txt
l add topic.txt
l save -qm 'fix: test change'
ok "add without running the configured queue" l queue add topic --into main --no-run-queue
eq "the entry remains pending" pending "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
eq "main is unchanged" "$base" "$(l rev-parse main)"
no "other commands refuse the option" l queue list --no-run-queue
ok "the queue can run separately" l queue run --into main
eq "the entry is merged" done "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"
finish
