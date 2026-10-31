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

# --store: every part of the store, and the bytes that only that part holds.
sync
st=$(l du --store --json)
part() { printf %s "$st" | python3 -c 'import json,sys; d=json.load(sys.stdin); rows={r["part"]: r for r in d["parts"]}; print(rows.get(sys.argv[1], {}).get(sys.argv[2], -1) if sys.argv[1] != "-" else d[sys.argv[2]])' "$1" "$2"; }
ok "--store lists the states of the project" test "$(part p/states frees)" -ge 0
ok "--store: the line's own file is freed by deleting it" test "$(part p/lines/w frees)" -ge $((4 * MiB))
ok "--store: the base file that states share is not main's alone" test "$(part p/lines/main frees)" -lt $((4 * MiB))
ok "--store: the total holds the base file and the own file once" test "$(part - total)" -ge $((8 * MiB))
ok "--store: the total does not count clones twice" test "$(part - total)" -lt $((40 * MiB))
has "--store prints the total" "store total" "$(l du --store)"
no "--store takes no line names" l du --store w
finish
