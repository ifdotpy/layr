#!/bin/bash
# Exact btrfs accounting through the real store report. The btrfs CI job runs all tests.
. "$(dirname "$0")/lib.sh"
set -e

mkdir "$T/src"
l init p --from "$T/src" -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd "$M/main"
MiB=1048576
# Keep fixture data incompressible and aligned to btrfs sectors, with no sparse holes.
random_file() { head -c "$1" /dev/urandom > "$2"; }
report() { sync; l du --store --json; }
value() {
    python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[sys.argv[1]] if len(sys.argv)==2 else next(p["frees"] for p in d["parts"] if p["part"]==sys.argv[2]))' "$@"
}
line_value() {
    python3 -c 'import json,sys; print(next(r[sys.argv[2]] for r in json.load(sys.stdin) if r["line"]==sys.argv[1]))' "$@"
}

# The saved snapshot shares the 4 MiB file with main. Compare the line's own
# totals with the empty line so project bookkeeping does not enter the result.
empty=$(l du --json)
random_file $((4 * MiB)) saved
l add saved
l save -qm saved
sync
saved_line=$(l du --json)
eq "saved file is counted in line total" $(( $(printf %s "$empty" | line_value main total) + 4 * MiB )) "$(printf %s "$saved_line" | line_value main total)"
eq "saved file is not unique to the line" "$(printf %s "$empty" | line_value main unique)" "$(printf %s "$saved_line" | line_value main unique)"
st=$(report)
ok "saved file is shared between line and states" test "$(printf %s "$st" | value shared)" -ge $((4 * MiB))

l line new -q other >/dev/null
# Add an unsaved file to main, then reflink it into the other line. Delta
# reports exclude every existing store file and must count this extent once.
base=$(report)
random_file $((2 * MiB)) source
sync
one=$(report)
eq "working file adds exactly 2 MiB to store total" $(( $(printf %s "$base" | value total) + 2 * MiB )) "$(printf %s "$one" | value total)"
eq "working file is freed with main" $(( $(printf %s "$base" | value frees p/lines/main) + 2 * MiB )) "$(printf %s "$one" | value frees p/lines/main)"
line_before_link=$(l du --json)
ln source source-link
sync
linked=$(report)
eq "hard link adds no store bytes" "$(printf %s "$one" | value total)" "$(printf %s "$linked" | value total)"
eq "hard link adds no line bytes" "$(printf %s "$line_before_link" | line_value main total)" "$(l du --json | line_value main total)"
cp --reflink=always source "$M/other/clone"
cloned=$(report)
eq "reflink adds no store bytes" "$(printf %s "$one" | value total)" "$(printf %s "$cloned" | value total)"
eq "reflink moves the extent to shared" $(( $(printf %s "$one" | value shared) + 2 * MiB )) "$(printf %s "$cloned" | value shared)"
eq "neither line frees the reflink" "$(printf %s "$base" | value frees p/lines/main)" "$(printf %s "$cloned" | value frees p/lines/main)"

# A plain store folder and a subvolume must use the same extent keys.
mkdir "$LAYR_ROOT/plain"
plain_base=$(report)
random_file $((2 * MiB)) plain-source
plain_one=$(report)
cp --reflink=always plain-source "$LAYR_ROOT/plain/clone"
plain_clone=$(report)
eq "plain-folder reflink adds no store bytes" "$(printf %s "$plain_one" | value total)" "$(printf %s "$plain_clone" | value total)"
eq "plain-folder reflink moves the extent to shared" $(( $(printf %s "$plain_one" | value shared) + 2 * MiB )) "$(printf %s "$plain_clone" | value shared)"
eq "line does not free the plain-folder reflink" "$(printf %s "$plain_base" | value frees p/lines/main)" "$(printf %s "$plain_clone" | value frees p/lines/main)"
eq "plain folder does not free the reflink" 0 "$(printf %s "$plain_clone" | value frees plain)"

# A range clone of the second half still pins the entire physical extent.
# Check the fixture is one extent before relying on that precise expectation.
random_file $((2 * MiB)) partial-source
sync
python3 - "$M/main/partial-source" "$M/other/partial" <<'PYCODE'
import fcntl, os, struct, sys
with open(sys.argv[1], 'rb') as src, open(sys.argv[2], 'wb') as dst:
    # FIEMAP header followed by room for two extents; the LAST flag proves one extent.
    buf = bytearray(32 + 2 * 56)
    struct.pack_into('=QQIIII', buf, 0, 0, 2**64 - 1, 0, 0, 2, 0)
    fcntl.ioctl(src.fileno(), 0xC020660B, buf, True)
    mapped = struct.unpack_from('=I', buf, 20)[0]
    logical, physical, length = struct.unpack_from('=QQQ', buf, 32)
    flags = struct.unpack_from('=I', buf, 32 + 40)[0]
    assert mapped == 1 and logical == 0 and physical and length == 2 * 1048576 and flags & 1, 'partial-clone source must be one 2 MiB extent'
    # FICLONERANGE: fd, source offset, length, destination offset.
    fcntl.ioctl(dst.fileno(), 0x4020940D, struct.pack('=qQQQ', src.fileno(), 1048576, 1048576, 0))
PYCODE
partial=$(report)
rm partial-source
partial_removed=$(report)
eq "partial clone keeps the whole extent alive" "$(printf %s "$partial" | value total)" "$(printf %s "$partial_removed" | value total)"
eq "remaining half clone owns the whole extent" $(( $(printf %s "$partial" | value frees p/lines/other) + 2 * MiB )) "$(printf %s "$partial_removed" | value frees p/lines/other)"

# Isolated store parts make the inline expectation independent of Layr's own
# small metadata files. All files are below the default btrfs max_inline.
btrfs subvolume create "$LAYR_ROOT/inline-a" >/dev/null
before_inline=$(report)
python3 - "$LAYR_ROOT/inline-a" <<'PY'
import os, pathlib, sys
p = pathlib.Path(sys.argv[1])
for i, n in enumerate((101, 203, 307)):
    (p / str(i)).write_bytes(os.urandom(n))
PY
inline=$(report)
# These assertions expose the current FIEMAP physical-address-zero collision:
# each distinct inline file must contribute its own logical payload exactly once.
eq "distinct inline files add 611 bytes" $(( $(printf %s "$before_inline" | value total) + 611 )) "$(printf %s "$inline" | value total)"
eq "inline part frees its 611 bytes" 611 "$(printf %s "$inline" | value frees inline-a)"
btrfs subvolume snapshot "$LAYR_ROOT/inline-a" "$LAYR_ROOT/inline-b" >/dev/null
snapshot=$(report)
eq "inline snapshot does not duplicate payload" "$(printf %s "$inline" | value total)" "$(printf %s "$snapshot" | value total)"
eq "inline snapshot moves 611 bytes to shared" $(( $(printf %s "$inline" | value shared) + 611 )) "$(printf %s "$snapshot" | value shared)"
eq "inline original frees no shared payload" 0 "$(printf %s "$snapshot" | value frees inline-a)"
eq "inline snapshot frees no shared payload" 0 "$(printf %s "$snapshot" | value frees inline-b)"

btrfs subvolume create "$LAYR_ROOT/unrelated-a" >/dev/null
btrfs subvolume create "$LAYR_ROOT/unrelated-b" >/dev/null
unrelated_before=$(report)
random_file 101 "$LAYR_ROOT/unrelated-a/file"
random_file 101 "$LAYR_ROOT/unrelated-b/file"
sync
eq "unrelated inline fixture uses equal inode numbers" "$(stat -c %i "$LAYR_ROOT/unrelated-a/file")" "$(stat -c %i "$LAYR_ROOT/unrelated-b/file")"
unrelated_after=$(report)
eq "unrelated inline files both count" $(( $(printf %s "$unrelated_before" | value total) + 202 )) "$(printf %s "$unrelated_after" | value total)"
eq "first unrelated inline file is private" 101 "$(printf %s "$unrelated_after" | value frees unrelated-a)"
eq "second unrelated inline file is private" 101 "$(printf %s "$unrelated_after" | value frees unrelated-b)"

# Compressed extents count their physical size. compsize reports that size in
# bytes when installed; otherwise only the nonzero, sub-logical bounds are known.
mkdir "$LAYR_ROOT/compressed"
if btrfs property set "$LAYR_ROOT/compressed" compression zstd 2>/dev/null; then
    before_compressed=$(report)
    head -c $((4 * MiB)) /dev/zero > "$LAYR_ROOT/compressed/data"
    sync
    compressed=$(report)
    allocated=$(printf %s "$compressed" | value frees compressed)
    if [ "$allocated" -gt 0 ] && [ "$allocated" -lt $((4 * MiB)) ]; then
        if command -v compsize >/dev/null 2>&1; then
            disk=$(compsize -b "$LAYR_ROOT/compressed/data" | awk '$1 == "TOTAL" {print $3}')
            eq "compressed data uses its physical allocation" "$disk" "$allocated"
        else
            echo "SKIP exact compression size: compsize unavailable"
        fi
        eq "compressed allocation is counted once" $(( $(printf %s "$before_compressed" | value total) + allocated )) "$(printf %s "$compressed" | value total)"
    else
        # Check independently whether compression was actually applied before skipping.
        blocks=$(( $(stat -c %b "$LAYR_ROOT/compressed/data") * 512 ))
        if [ "$blocks" -gt 0 ] && [ "$blocks" -lt $((4 * MiB)) ]; then
            ok "compressed allocation is nonzero and below logical size" false
        else
            echo "SKIP compression: filesystem did not compress data"
        fi
    fi
else
    echo "SKIP compression: zstd property unavailable"
fi
finish
