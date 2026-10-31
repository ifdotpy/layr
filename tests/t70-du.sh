#!/bin/bash
# `layr du`: the disk space of each line and the part that only that line holds, on every
# backend (APFS private size, btrfs and XFS/ext4 shared extents, OverlayFS upper layers).
. "$(dirname "$0")/lib.sh"

sample $T/src
head -c 4194304 /dev/urandom > $T/src/base.bin
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd $M/main || exit 1
l line new -q w >/dev/null
MiB=1048576
field() { python3 -c 'import json,sys; rows={r["line"]: r for r in json.load(sys.stdin)}; print(rows[sys.argv[1]][sys.argv[2]])' "$1" "$2"; }

sync
out=$(l du --json)
ok "a new line holds the base file" test "$(printf %s "$out" | field w total)" -ge $((4 * MiB))
ok "a new line shares it: almost nothing is its own" test "$(printf %s "$out" | field w unique)" -lt $MiB

cd $M/w || exit 1
head -c 4194304 /dev/urandom > own.bin
ln own.bin own-link.bin
sync
out=$(l du --json)
u=$(printf %s "$out" | field w unique)
ok "a new file is the line's own" test "$u" -ge $((4 * MiB))
ok "a second link does not count twice" test "$u" -lt $((6 * MiB))
ok "the other line does not hold it" test "$(printf %s "$out" | field main unique)" -lt $((4 * MiB))

eq "du <line> lists only that line" "w" "$(l du --json w | python3 -c 'import json,sys; print(" ".join(r["line"] for r in json.load(sys.stdin)))')"
no "an unknown line is refused" l du nosuch
has "the table has a header" "unique" "$(l du)"
has "the table has the sum" "all lines" "$(l du)"
finish
