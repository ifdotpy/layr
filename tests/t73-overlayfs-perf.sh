#!/bin/bash
# OverlayFS cost contracts: a long save in one project does not hold up another project,
# a clean status reads no file content, and a send with a parent is incremental.
if [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    echo "SKIP: this test needs the OverlayFS backend (LAYR_OVERLAYFS_TEST_ROOT)"
    echo "t73-overlayfs-perf.sh: 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"

now_ms() { echo $(($(date +%s%N) / 1000000)); }
# The longest span of one kind in a LAYR_TRACE log, in milliseconds.
longest() { awk -v name="$2" 'index($0, name) { if ($2 + 0 > m) m = $2 + 0 } END { print m + 0 }' "$1"; }

# --- Two projects save at the same time -------------------------------------------------
mkdir -p $T/big
python3 - $T/big <<'PY'
import os, sys
root = sys.argv[1]
for d in range(100):
    os.makedirs(f"{root}/d{d:03d}")
    for i in range(300):
        with open(f"{root}/d{d:03d}/f{i:03d}", "wb") as f:
            f.write(bytes([i % 251]) * 4096)
PY
sample $T/small
l init a --from $T/big -q >/dev/null
l init b --from $T/small -q >/dev/null
A=$LAYR_ROOT/projects/a/lines
B=$LAYR_ROOT/projects/b/lines
cd $A/main
python3 - <<'PY'
import os
for d in range(100):
    for i in range(300):
        with open(f"d{d:03d}/f{i:03d}", "wb") as f:
            f.write(bytes([(i + 7) % 251]) * 4096)
PY
rm -f $T/a.done $T/a.ms
(
    cd $A/main
    start=$(now_ms)
    LAYR_TRACE=1 l save -qam "rewrite everything" 2> $T/a.trace
    echo $(($(now_ms) - start)) > $T/a.ms
    touch $T/a.done
) &
# B starts when A has passed its first registry step and copies without the lock.
for _ in $(seq 1 200); do
    grep -q "registry lock held" $T/a.trace 2>/dev/null && break
    sleep 0.05
done
cd $B/main
echo more >> notes.txt
LAYR_TRACE=1 l save -qam "small save" 2> $T/b.trace
ok "project B saved" test "$(l log -1 --format=%s)" = "small save"
if [ -e $T/a.done ]; then a_running=no; else a_running=yes; fi
eq "project B finished while the long save in project A was still running" "yes" "$a_running"
wait
total=$(cat $T/a.ms)
hold=$(longest $T/a.trace "OverlayFS registry lock held")
ok "A took long enough to measure (${total} ms)" test "$total" -ge 1000
ok "A holds the registry lock for less than half of its save (longest ${hold} ms)" awk -v h="$hold" -v t="$total" 'BEGIN { exit !(h * 2 < t) }'
ok "B never waited long for the registry lock ($(longest $T/b.trace "OverlayFS registry lock wait") ms)" \
    awk -v w="$(longest $T/b.trace "OverlayFS registry lock wait")" 'BEGIN { exit !(w < 1000) }'
cd $A/main
eq "A's save is complete" "rewrite everything" "$(l log -1 --format=%s)"
ok "A's fsck passes" l fsck

# --- A clean status reads no file content -----------------------------------------------
cd $A/main
l line new -q w >/dev/null
cd $A/w
LAYR_TRACE=1 l status --porcelain > $T/status.out 2> $T/status.trace
eq "status of a clean line is empty" "" "$(cat $T/status.out)"
eq "a clean status compares no content" "0" "$(grep -c 'content compare' $T/status.trace)"
# A write that copies a file up but keeps its content and times is no change.
chmod 644 d001/f001
LAYR_TRACE=1 l status --porcelain > $T/status.out 2> $T/status.trace
eq "a copied-up file with equal size and times is not read" "0" "$(grep -c 'content compare' $T/status.trace)"
no "and it is not reported" grep -q f001 $T/status.out
# Same size, new content: the file is read and reported.
printf 'ZZ' | dd of=d002/f002 bs=1 seek=10 conv=notrunc status=none
LAYR_TRACE=1 l status --porcelain > $T/status.out 2> $T/status.trace
has "a changed file is reported" "d002/f002" "$(cat $T/status.out)"
ok "only the changed file is read" test "$(grep -c 'content compare' $T/status.trace)" -ge 1
rm d003/f003
echo new > added.txt
l status --porcelain > $T/status.out
has "a deleted file is reported" "d003/f003" "$(cat $T/status.out)"
has "a new file is reported" "added.txt" "$(cat $T/status.out)"
l add added.txt
l save -qam "delete, add and change" >/dev/null
eq "status is clean after the save" "" "$(l status --porcelain)"

# --- A send with a parent is incremental ------------------------------------------------
mkdir -p $T/s
python3 - $T/s <<'PY'
import os, sys
root = sys.argv[1]
os.makedirs(f"{root}/sub")
for i in range(300):
    with open(f"{root}/sub/f{i:03d}", "wb") as f:
        f.write(os.urandom(65536))
PY
l init s --from $T/s -q >/dev/null
S=$LAYR_ROOT/projects/s/lines
cd $S/main
echo one >> sub/f001; l save -qam s1
BR=$T/peer/store
mkdir -p $T/peer
peer() { echo env LAYR_ROOT=$BR $LAYR serve-peer s; }
peer_exchange sync s -q -- $(peer)
echo two >> sub/f002; rm sub/f003; echo fresh > sub/new.txt; mkdir sub/dir; echo deep > sub/dir/x
l save -qam s2
R=$(l rev-parse head)
peer_exchange sync s -q -- $(peer)
eq "the peer holds the incremental state" "$(l rev-parse "$R^{tree}")" "$(env LAYR_ROOT=$BR $LAYR --project s rev-parse "$R^{tree}" 2>&1)"
# The two newest sealed states: the second was saved after the first, in the same line.
set -- $(ls -1t $LAYR_ROOT/projects/s/states | grep -v @ | head -2)
child=$LAYR_ROOT/projects/s/states/$1
parent=$LAYR_ROOT/projects/s/states/$2
uuid=$(python3 - "$LAYR_ROOT" "$parent" <<'PY'
import json, sys
info = json.load(open(f"{sys.argv[1]}/.layr-overlayfs/registry.json"))["trees"][sys.argv[2]]["info"]
print(info.get("received_uuid") or info["uuid"])
PY
)
ok "the parent state is known" test -n "$uuid"
full=$($LAYR __apfs-send "$child" | wc -c)
incr=$($LAYR __apfs-send "$child" "$parent" "$uuid" | wc -c)
ok "the full stream holds the tree (${full} bytes)" test "$full" -ge 10000000
ok "the incremental stream is small (${incr} bytes)" test "$incr" -lt 200000

finish
