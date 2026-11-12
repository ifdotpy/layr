#!/bin/bash
# ci: macos overlayfs
# #174: a folder that .layrattributes marks regenerable, and the ignore rules ignore, is a cache
# from the first automatic save. Its files stay out of every state without `save --overlays`, and
# the layer is kept by layr itself. A new line from a state still starts warm (the cache overlay
# is taken), for the same user.
. "$(dirname "$0")/lib.sh"

S=$T/src
rm -rf $S && mkdir -p $S/src
echo code > $S/src/main.rs
printf 'target/\n' > $S/.gitignore
printf 'target regenerable\n' > $S/.layrattributes
l init r --from $S -q >/dev/null
P=$LAYR_ROOT/projects/r/lines/main
cd $P || exit 1

# A build made target/ before the first automatic save: 50 MB of build output.
mkdir -p target
head -c 52428800 /dev/urandom > target/blob.bin
blob_sum=$(cksum < target/blob.bin)

# 20 automatic saves. Each changes 5 MB in target and one tracked file. No `save --overlays`.
for i in $(seq 1 20); do
    head -c 5242880 /dev/urandom > target/churn.bin
    echo "change $i" >> src/main.rs
    l save -q >/dev/null || { echo "FAIL: automatic save $i"; exit 1; }
done

# The store grows by the tracked changes, one live target (55 MB) and at most one first cache state
# (50 MB, a copy on overlayfs, a clone on APFS and btrfs). Not 20 copies: that is over 1 GB.
store_bytes() {
    # "store total   50.3 MiB" in `layr du --store`; convert to bytes.
    l du --store | awk '/^store total/ { v = $(NF-1); u = $NF;
        m = 1; if (u == "KiB") m = 1024; else if (u == "MiB") m = 1048576; else if (u == "GiB") m = 1073741824;
        printf "%d\n", v * m }'
}
l gc --repair -q >/dev/null 2>&1
growth=$(store_bytes)
limit=$((130 * 1048576))  # live target 55 MB + first cache state 50 MB; baseline is about 150 MB
if [ "$growth" -lt "$limit" ]; then
    echo "ok 1 store grows by about one live target copy ($growth bytes)"
    PASS=$((PASS + 1))
else
    echo "not ok 1 store grows by $growth bytes, limit $limit (about 20 copies of target)"
    FAIL=$((FAIL + 1))
fi

# A cache folder is no content of a state: no automatic state lists a file of target.
states=$(l log --auto | awk '/^state /{print $2}')
held=0
for s in $states; do
    if [ -n "$(l show "$s:target/churn.bin" 2>/dev/null)" ] || [ -n "$(l show "$s:target/blob.bin" 2>/dev/null)" ]; then
        held=$((held + 1))
    fi
done
eq "no automatic state holds a target file" 0 "$held"
no "the head state has no target file" sh -c "$LAYR show $(l rev-parse):target/churn.bin >/dev/null 2>&1"

# The cache overlay exists without save --overlays.
ok "the target cache overlay is made by layr" sh -c "$LAYR overlay list | grep -q cache"

# A new line from an automatic state starts warm: it has the blob of the first overlay state.
first=$(l log --auto | awk '/^state /{print $2}' | tail -1)
l line new -q warm "$first"
W=$LAYR_ROOT/projects/r/lines/warm
ok "a new line from such a state starts warm" test -f $W/target/blob.bin
eq "its cache has the build output" "$blob_sum" "$(cksum < $W/target/blob.bin)"

# Two more new lines from main take the saved cache: no new cache state, and no copy of the cache
# (OverlayFS mounts the taken state as the lower layer of the line's cache folder).
cd $P || exit 1
states_of_cache() { ls "$LAYR_ROOT/projects/r/meta/overlays/$TG/states" | wc -l; }
states_before=$(states_of_cache)
before=$(store_bytes)
l line new -q warm2 main
l line new -q warm3 main
after=$(store_bytes)
echo "info: store growth of two new lines from main: $((after - before)) bytes"
echo "info: layr du after the two new lines:"; l du 2>&1 | sed "s/^/info: /"
ok "two new lines from main add under 1 MiB of store growth" test $((after - before)) -lt 1048576
eq "two new lines from main make no cache state" "$states_before" "$(states_of_cache)"
cd $LAYR_ROOT/projects/r/lines/warm2 || exit 1
ok "and they start warm" test -f target/blob.bin
cd $P || exit 1
# Release: deleting a new line drops its cache (the mount on OverlayFS) and keeps main's cache.
l line delete -D warm2 >/dev/null
l line delete -D warm3 >/dev/null
ok "deleting the new lines removes their folders" sh -c "! test -e $LAYR_ROOT/projects/r/lines/warm2 && ! test -e $LAYR_ROOT/projects/r/lines/warm3"
ok "main keeps its cache after they are gone" test -f $P/target/blob.bin
ok "the free step runs after the release" l gc --repair -q

# A named save makes the first cache state too, with no automatic save before it.
N=$T/named
rm -rf $N && mkdir -p $N/src
echo code > $N/src/main.rs
printf 'target/\n' > $N/.gitignore
printf 'target regenerable\n' > $N/.layrattributes
l init n --from $N -q >/dev/null
cd $LAYR_ROOT/projects/n/lines/main || exit 1
mkdir -p target && head -c 1048576 /dev/urandom > target/built
echo named >> src/main.rs
l save -a -m named >/dev/null
no "a named state holds no target file" sh -c "$LAYR show $(l rev-parse):target/built >/dev/null 2>&1"
ok "a named save makes the cache overlay" sh -c "$LAYR overlay list | grep -q cache"
l line new -q nwarm "$(l rev-parse)"
ok "a new line from the named state starts warm" test -f $LAYR_ROOT/projects/n/lines/nwarm/target/built
finish
