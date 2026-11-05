#!/bin/bash
# ci: overlayfs
# A save never swaps a cache folder that a process uses. While a writer holds a file open in target/,
# saves leave target/ as it is (same inode, no swap folder, not a cache), and an explicit save
# --caches refuses it with the pid. A process that only reads target/ does not stop a save: after
# the writer stops, a save makes target/ a cache while the reader keeps its file open.
# Linux only: the btrfs CI job runs every test, the OverlayFS job runs this one.
if [ "$(uname -s)" != Linux ]; then
    echo "t118-cache-busy.sh: not a Linux run, 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"

# A cache is a subvolume (btrfs) or a layer in the OverlayFS registry.
REG="$LAYR_ROOT/.layr-overlayfs/registry.json"
is_cache() { # is_cache <folder>
    if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
        python3 -I -c 'import json, sys; r = json.load(open(sys.argv[1])); sys.exit(0 if any(p.endswith("/lines/w/target") for p in r["trees"]) else 1)' "$REG"
    else
        btrfs subvolume show "$1" >/dev/null 2>&1
    fi
}

sample $T/src
l init cb --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/cb/lines
cd $P/main
# Line w is made before target is a cache: its target is a plain folder. (A line made from a cache
# gets a cache layer, and rm -rf keeps the layer mounted on OverlayFS.) The busy check looks at
# plain folders; a cache layer is saved as it is.
l line new -q w
l config regenerable '["target"]'
W=$P/w
cd $W
rm -rf target
mkdir -p target/debug
no "the build starts with a plain target folder" is_cache $W/target

# A build: the writer holds the cargo lock open and keeps writing, like cargo does.
(exec 3>target/debug/.cargo-lock; while :; do date >> target/debug/out; sleep 0.05; done) &
WRITER=$!
echo "diag: writer pid $WRITER"
ls -l /proc/$WRITER/fd || true
echo "diag: writer cwd -> $(readlink /proc/$WRITER/cwd)"
echo "diag: target canonical -> $(readlink -f $W/target)"
ok "the writer holds target/debug/.cargo-lock open" sh -c "ls -l /proc/$WRITER/fd | grep -q 'target/debug/.cargo-lock'"
sleep 1
ino=$(stat -c %i target)
size1=$(stat -c %s target/debug/out)
l save -q
eq "a save keeps the inode of target" "$ino" "$(stat -c %i target)"
eq "a save makes no swap folder" "" "$(ls -d target.layr-* 2>/dev/null)"
no "a save keeps target a plain folder" is_cache $W/target
sleep 1
size2=$(stat -c %s target/debug/out)
ok "the writer keeps writing into target" test "$size2" -gt "$size1"
out=$(LAYR_TRACE=1 l save --caches --cache target 2>&1)
rc=$?
echo "diag: busy trace: $(echo "$out" | grep -i busy | tr '\n' ' ')"
eq "an explicit save refuses the busy folder" "1" "$rc"
has "the refusal names the pid" "in use by pid $WRITER (" "$out"
has "the refusal says the pid writes" "writes $(readlink -f $W/target)/debug/" "$out"
has "the refusal says when to run again" "run again when the build ends" "$out"
eq "the refused save keeps the inode" "$ino" "$(stat -c %i target)"
eq "a refused save makes no swap folder" "" "$(ls -d target.layr-* 2>/dev/null)"
no "target is not a cache while the build runs" is_cache $W/target

# A reader, like a service that scans the folder, holds a file open for reading only.
(exec 3<target/debug/out; exec sleep 60) &
READER=$!
kill $WRITER 2>/dev/null
wait $WRITER 2>/dev/null
sleep 1
ok "the reader holds target/debug/out open" sh -c "ls -l /proc/$READER/fd | grep -q 'target/debug/out'"
l save -q
ok "after the build, a save makes target a cache while a reader runs" is_cache $W/target
kill $READER 2>/dev/null
wait $READER 2>/dev/null
eq "no swap folder is left" "" "$(ls -d target.layr-* 2>/dev/null)"
ok "the build output is kept" test -s $W/target/debug/out
finish
