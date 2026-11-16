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
# A tight loop without external commands: every round of the copy sees a new version.
(n=0; while :; do n=$((n + 1)); echo "$n" > hot.txt; echo "$n" > hot2.txt; echo "$n" > data/f1; done) &
WRITER=$!
trap 'kill $WRITER 2>/dev/null' EXIT
sleep 1
# On a busy host (check suites run at once) the writer can miss a whole copy round, and that copy then
# settles (overlayfs-ext4, 2026-11-15). Each try is a new save; one that did not settle is enough.
for try in 1 2 3 4 5; do
    out=$(l save 2>&1)
    rc=$?
    [ "$rc" -eq 0 ] || break
    printf '%s\n' "$out" | grep -q 'kept changing' && break
    echo "the copy settled on try $try; saving again"
done
kill $WRITER 2>/dev/null; wait $WRITER 2>/dev/null
eq "the save succeeds while the file changes" "0" "$rc"
has "one line tells that the copy did not settle" "kept changing while it was copied" "$out"
eq "that line is printed once" "1" "$(printf '%s\n' "$out" | grep -c 'kept changing')"
ok "the state is marked as taken while processes ran" sh -c "'$LAYR' log --auto -3 | grep -q 'taken while processes ran'"
finish
