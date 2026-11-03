#!/bin/bash
# ci: overlayfs
# A save never swaps a cache folder that a process uses. While a writer holds a file open in target/,
# saves leave target/ as it is (same inode, no swap folder, not a cache), and an explicit save
# --caches refuses it with the pid. After the writer stops, a save makes target/ a cache.
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
l config regenerable '["target"]'
l line new -q w
W=$P/w
cd $W

# A build: the writer holds the cargo lock open and keeps writing, like cargo does.
mkdir -p target/debug
(exec 3>target/debug/.cargo-lock; while :; do date >> target/debug/out; sleep 0.05; done) &
WRITER=$!
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
out=$(l save --caches --cache target 2>&1)
eq "an explicit save refuses the busy folder" "1" "$?"
has "the refusal names the pid" "in use by pid $WRITER (" "$out"
has "the refusal says when to run again" "run again when the build ends" "$out"
eq "the refused save keeps the inode" "$ino" "$(stat -c %i target)"
eq "a refused save makes no swap folder" "" "$(ls -d target.layr-* 2>/dev/null)"
no "target is not a cache while the build runs" is_cache $W/target

kill $WRITER 2>/dev/null
wait $WRITER 2>/dev/null
sleep 1
l save -q
ok "after the build, a save makes target a cache" is_cache $W/target
eq "no swap folder is left" "" "$(ls -d target.layr-* 2>/dev/null)"
ok "the build output is kept" test -s $W/target/debug/out
finish
