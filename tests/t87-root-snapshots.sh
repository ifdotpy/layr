#!/bin/bash
# Root snapshot guard (btrfs). A store in the root subvolume is refused unless overridden, a
# snapper config that covers the store refuses it too, and a line or state folder that a rollback
# left empty stops the commands of its project.
. "$(dirname "$0")/lib.sh"

if [ "$(uname -s)" != Linux ] || [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    echo "t87-root-snapshots.sh (btrfs only): 0 passed, 0 failed"
    exit 0
fi

# The test file system is mounted at $LAYR_ROOT: its top level is the subvolume with id 5.
M=$LAYR_ROOT
sample $T/src
export LAYR_ROOT_SUBVOLUME_PATH=$M
export LAYR_SNAPPER_CONFIGS=$T/no-snapper

# A store folder in the top-level subvolume is the root subvolume's content.
export LAYR_ROOT=$M/plain
out=$(l init p --from $T/src -q 2>&1); rc=$?
ok "a store in the root subvolume is refused" test "$rc" -ne 0
has "the refusal explains the rollback risk" "reverts the plain files" "$out"
has "the refusal names the fix" "docs/btrfs-root-snapshots.md" "$out"
no "the refused store is not created" test -e $M/plain
out=$(l init p --from $T/src -q --allow-root-snapshots 2>&1); rc=$?
eq "--allow-root-snapshots lets init pass" 0 "$rc"
ok "the store exists after the override" test -d $M/plain/projects/p
ok "LAYR_ALLOW_ROOT_SNAPSHOTS=1 lets init pass" env LAYR_ALLOW_ROOT_SNAPSHOTS=1 "$LAYR" init p2 --from $T/src -q

# A store in its own subvolume is normal.
btrfs subvolume create $M/own >/dev/null
export LAYR_ROOT=$M/own
ok "a store in its own subvolume passes init" l init q --from $T/src -q
ok "a normal store passes status" sh -c "cd $M/own/projects/q/lines/main && $LAYR status >/dev/null"

# A snapper config that covers the store refuses it. A config for the top level does not cover
# a store that is its own subvolume.
mkdir -p $T/snapper
printf 'SUBVOLUME="%s"\n' "$M" > $T/snapper/root
export LAYR_SNAPPER_CONFIGS=$T/snapper
export LAYR_ROOT_SUBVOLUME_PATH=$M/own
export LAYR_ROOT=$M/plain2
mkdir -p $M/plain2
out=$(l init p3 --from $T/src -q 2>&1); rc=$?
ok "a snapper config that covers the store refuses init" test "$rc" -ne 0
has "the snapper refusal names the config" "snapper config" "$out"
export LAYR_ROOT_SUBVOLUME_PATH=$M
export LAYR_ROOT=$M/own
ok "a snapper config of the top level does not cover a subvolume" sh -c "cd $M/own/projects/q/lines/main && $LAYR status >/dev/null"

# A line folder that a rollback emptied stops every command of its project.
P=$M/own/projects/q/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new w >/dev/null
btrfs subvolume delete $P/w
mkdir $P/w
out=$(cd $P/main && $LAYR status 2>&1); rc=$?
ok "a rolled back line refuses status" test "$rc" -ne 0
has "the rollback message" "rolled back by a system snapshot" "$out"
has "the rollback message names the recovery" "subvolid=5" "$out"
out=$(cd $P/main && $LAYR line list 2>&1); rc=$?
ok "a rolled back line refuses line list" test "$rc" -ne 0
has "line list shows the rollback message" "rolled back by a system snapshot" "$out"

# A state folder that a rollback emptied stops its project too.
l init r --from $T/src -q >/dev/null
R=$M/own/projects/r
ok "a normal project passes status" sh -c "cd $R/lines/main && $LAYR status >/dev/null"
S=$(ls $R/states | head -1)
btrfs subvolume delete $R/states/$S
mkdir $R/states/$S
out=$(cd $R/lines/main && $LAYR status 2>&1); rc=$?
ok "an emptied state refuses status" test "$rc" -ne 0
has "the state message" "rolled back by a system snapshot" "$out"

finish
