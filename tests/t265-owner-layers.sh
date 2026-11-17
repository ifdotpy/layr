#!/bin/bash
# ci: macos overlayfs
# Private imports and stages retain the line owner before immutable snapshots are shared.
# The privileged companion native-owner-layers-265.py additionally inspects physical OverlayFS uppers.
# Legacy Linux CI launches shell tests as root. The lead's isolated service harness
# runs this file as uid 1000; never turn its privileged controller into a client.
[ "$(id -u)" != 0 ] || {
    echo 't265-owner-layers.sh (requires an unprivileged client/service harness): 0 passed, 0 failed'
    exit 0
}
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src/files"
for i in $(seq 1 100); do printf 'file %s\n' "$i" > "$T/src/files/$i"; done
ln "$T/src/files/100" "$T/src/twin"
ln -s files/100 "$T/src/link"
l init owners --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/owners/lines
cd "$P/main"
l protect main --remove >/dev/null || exit 1
for i in $(seq 1 10); do printf 'edited %s\n' "$i" > "files/$i"; done
l add files/{1,2,3,4,5,6,7,8,9,10} || exit 1
l save -qm 'ten changed files' >/dev/null || exit 1
l line new same main -q >/dev/null || exit 1
cd "$P/same"
eq 'unchanged contents survive the shared snapshot' 'file 100' "$(cat files/100)"
eq 'staged contents survive the shared snapshot' 'edited 1' "$(cat files/1)"
eq 'symlinks stay links' files/100 "$(readlink link)"
eq 'hard links stay one inode' "$(stat_field inode files/100)" "$(stat_field inode twin)"
eq 'every inherited file belongs to the line owner' 0 "$(find files -type f ! -user "$(id -u)" | wc -l | tr -d ' ')"
finish
