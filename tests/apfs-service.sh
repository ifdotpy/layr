#!/bin/bash
# Run with sudo. Uses the existing caller and nobody; creates no system accounts.
set -eu
[ "$(uname -s)" = Darwin ] && [ "$(id -u)" = 0 ] || { echo 'Run this test as root on macOS'; exit 1; }
: "${LAYR_APFS_TEST_ROOT:?Set an isolated case-sensitive APFS test volume}"
: "${LAYR_APFS_TEST_USER:?Set the existing non-root test user}"
[ "$(id -u "$LAYR_APFS_TEST_USER")" != 0 ] || exit 1
. "$(dirname "$0")/lib.sh"
chmod 755 "$T" "$LAYR_ROOT"
cp "$LAYR" "$T/layr"
L=$T/layr
export LAYR_SOCKET=$T/layr.sock
"$L" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill "$DPID" 2>/dev/null || true; wait "$DPID" 2>/dev/null || true; cleanup' EXIT
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
[ -S "$LAYR_SOCKET" ] || { cat "$T/daemon.log"; exit 1; }
as() { local user=$1; shift; sudo -u "$user" env LAYR_SOCKET="$LAYR_SOCKET" "$@"; }
sample "$T/src"
l init service --from "$T/src" --owner "$LAYR_APFS_TEST_USER" -q >/dev/null
P=$LAYR_ROOT/projects/service/lines
cd "$P/main"
ok 'the owner reads through the service' as "$LAYR_APFS_TEST_USER" "$L" status -s
as "$LAYR_APFS_TEST_USER" /bin/chmod +a "everyone allow list,search,readattr" "$P/main"
ok 'the owner creates a line' as "$LAYR_APFS_TEST_USER" "$L" branch -q work
cd "$P/work"
as "$LAYR_APFS_TEST_USER" sh -c 'echo changed > file.txt'
ok 'the owner stages a file' as "$LAYR_APFS_TEST_USER" "$L" add file.txt
ok 'the owner commits through the service' as "$LAYR_APFS_TEST_USER" "$L" commit -qm service
no 'the owner cannot read stored states directly' as "$LAYR_APFS_TEST_USER" ls "$LAYR_ROOT/projects/service/states"
no 'another user cannot read the line' as nobody cat "$P/work/file.txt"
cd /private/tmp
no 'another user cannot read private history through the service' as nobody env LAYR_PROJECT=service LAYR_LINE=work "$L" show HEAD:file.txt
cd "$P/work"
/usr/bin/id "$LAYR_APFS_TEST_USER"
check_identity() {
    as "$LAYR_APFS_TEST_USER" "$L" try HEAD -- /bin/sh -c "test \"\$(/usr/bin/id -u)\" = $(id -u "$LAYR_APFS_TEST_USER")" > "$T/check.log" 2>&1
}
ok 'a check runs with the caller identity' check_identity
cat "$T/check.log"
eq 'a client cannot restore through the root service' '1' "$(as "$LAYR_APFS_TEST_USER" "$L" backup restore /private/tmp/nonexistent-layr-backup >/dev/null 2>&1; echo $?)"
eq 'a client cannot serve peers through the root service' '1' "$(as "$LAYR_APFS_TEST_USER" "$L" serve-peer service </dev/null >/dev/null 2>&1; echo $?)"
ok 'service snapshots pass fsck' as "$LAYR_APFS_TEST_USER" "$L" fsck
finish
