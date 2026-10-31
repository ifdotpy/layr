#!/bin/bash
# Names that differ only in Unicode normalization (cafe with a combining accent, and the
# precomposed form) are two files on btrfs, ext4 and XFS, but one entry on APFS, also on a
# case-sensitive volume. Writing both there would lose one: the write is refused first.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git
git init -q --bare -b main $R
# Plumbing only: a working folder on APFS cannot hold both names.
nfc=$(printf 'caf\xc3\xa9.txt'); nfd=$(printf 'cafe\xcc\x81.txt')
b0=$(echo base | git -C $R hash-object -w --stdin)
b1=$(echo one | git -C $R hash-object -w --stdin)
b2=$(echo two | git -C $R hash-object -w --stdin)
tree=$(printf '100644 blob %s\tkeep.txt\n100644 blob %s\t%s\n100644 blob %s\t%s\n' $b0 $b1 "$nfc" $b2 "$nfd" | git -C $R mktree)
git -C $R update-ref refs/heads/main $(git -C $R commit-tree $tree -m both)

if [ "$(uname -s)" = Darwin ]; then
    out=$(l git clone $R p -q 2>&1); rc=$?
    eq "APFS refuses both names" 1 "$([ $rc -ne 0 ] && echo 1 || echo 0)"
    has "the message names the normalization" "Unicode normalization" "$out"
else
    l git clone $R p -q >/dev/null
    cd $LAYR_ROOT/projects/p/lines/main || exit 1
    eq "both names are files" 3 "$(ls | wc -l | tr -d ' ')"
fi
finish
