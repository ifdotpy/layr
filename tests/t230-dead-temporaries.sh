#!/bin/bash
# ci: macos overlayfs
# A temporary copy of a process that is gone (a crash, a killed migration) leaves meta/work at the next
# command, without `layr gc --repair`. A copy of a live process stays.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
W=$LAYR_ROOT/projects/p/meta/work
mkdir -p "$W/01a1c1cc-17e2-7512-9e71-ceeaf1120e65.2000000000/deep/folder" "$W/01a1c1cc-17e2-7512-9e71-ceeaf1120e66.$$"
echo data > "$W/01a1c1cc-17e2-7512-9e71-ceeaf1120e65.2000000000/deep/folder/file"
l status >/dev/null
ok "the next command removes the copy of a dead process" test ! -e "$W/01a1c1cc-17e2-7512-9e71-ceeaf1120e65.2000000000"
ok "the copy of a live process stays" test -d "$W/01a1c1cc-17e2-7512-9e71-ceeaf1120e66.$$"
rmdir "$W/01a1c1cc-17e2-7512-9e71-ceeaf1120e66.$$"
finish
