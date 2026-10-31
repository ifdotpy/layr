#!/bin/bash
# A save while a process keeps writing a file: the copy cannot settle, so the state is kept and
# marked as taken while processes ran (APFS and OverlayFS).
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init busy --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/busy/lines/main" || exit 1
l line new -q work
cd "$LAYR_ROOT/projects/busy/lines/work" || exit 1
mkdir -p data && for i in $(seq 1 50); do printf '%s\n' "$i" > "data/f$i"; done
printf 'start\n' > hot.txt
(while :; do date +%s%N > hot.txt; done) &
WRITER=$!
trap 'kill $WRITER 2>/dev/null' EXIT
sleep 1
out=$(l save 2>&1)
rc=$?
kill $WRITER 2>/dev/null; wait $WRITER 2>/dev/null
eq "the save succeeds while the file changes" "0" "$rc"
has "one line tells that the copy did not settle" "kept changing while it was copied" "$out"
eq "that line is printed once" "1" "$(printf '%s\n' "$out" | grep -c 'kept changing')"
ok "the state is marked as taken while processes ran" sh -c "'$LAYR' log --auto -3 | grep -q 'taken while processes ran'"
finish
