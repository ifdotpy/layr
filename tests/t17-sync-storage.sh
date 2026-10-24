#!/bin/bash
# Extra clone sources and compressed extents, through the actual sync protocol.
. "$(dirname "$0")/lib.sh"
set -e
mkdir "$T/src"
echo initial > "$T/src/f"
l init perf --from "$T/src" -q
P=$LAYR_ROOT/projects/perf
head_id() { l rev-parse head | python3 -c 'import sys, uuid; s=sys.stdin.read().strip(); print(uuid.UUID(hex=s[20:]+s[12:20]+s[:12]))'; }
cd "$P/lines/main" || exit 1
python3 - <<'PY'
import random
with open('blob', 'wb') as f:
    r = random.Random(42)
    for _ in range(128):
        f.write(r.randbytes(128 * 1024))
PY
l add blob
l save -qm blob
first=$(head_id)
rm blob
l save -qam remove
cp --reflink=always "$P/states/$first/blob" blob
l add blob
l save -qm return
last=$(head_id)
new_fs "$T/peer" 2G
BR=$T/peer/store
# Separate filesystem transaction numbers must not accidentally match in a return trip.
for i in {1..8}; do
    btrfs subvolume create "$T/peer/clock-$i" >/dev/null
    btrfs subvolume delete "$T/peer/clock-$i" >/dev/null
done
ok "sync restores content from an extra source" peer_exchange sync perf -q -- bash -c "tee '$T/clone.wire' | env LAYR_ROOT='$BR' '$LAYR' serve-peer perf"
ok "received content matches the original" cmp blob "$BR/projects/perf/states/$last/blob"
btrfs filesystem sync "$T/peer" >/dev/null
# The receiver must share the restored extent with the earlier state.
read -r _total exclusive _shared _name < <(btrfs filesystem du --raw "$BR/projects/perf/states/$last/blob" | tail -1)
eq "the restored blob has no exclusive data extents" 0 "$exclusive"
# New files on the source use native compression. The receiver negotiates protocol 2.
btrfs property set . compression zstd
python3 - <<'PY'
with open('compressed', 'wb') as f:
    for _ in range(128):
        f.write(b'source text\n' * 12000)
PY
l add compressed
l save -qm compressed
last=$(head_id)
ok "sync transfers compressed extents" peer_exchange sync perf -q -- bash -c "tee '$T/compressed.wire' | env LAYR_ROOT='$BR' '$LAYR' serve-peer perf"
ok "compressed content restores exactly" cmp compressed "$BR/projects/perf/states/$last/compressed"
# Inspect the actual bytes sent by layr, not a separately constructed stream.
ok "wire uses encoded writes and external clone commands" python3 - "$T/clone.wire" "$T/compressed.wire" <<'PY'
import pathlib, struct, sys

def commands(path):
    wire = pathlib.Path(path).read_bytes()
    offset = 0
    stream = bytearray()
    result = []
    while offset < len(wire):
        kind = wire[offset:offset+1]
        size = struct.unpack_from('>I', wire, offset + 1)[0]
        data = wire[offset+5:offset+5+size]
        offset += 5 + size
        if kind == b'D':
            stream.extend(data)
        elif kind == b'E':
            assert stream[:13] == b'btrfs-stream\0'
            pos = 17
            while pos < len(stream):
                length, code = struct.unpack_from('<IH', stream, pos)
                result.append(code)
                pos += 10 + length
            assert pos == len(stream)
            stream.clear()
    return result
assert 16 in commands(sys.argv[1]), 'no clone command'
assert 25 in commands(sys.argv[2]), 'no encoded write command'
PY
# A third machine has the original states, received independently from the first.
new_fs "$T/third" 2G
CR=$T/third/store
peer_exchange sync perf -q -- env LAYR_ROOT="$CR" "$LAYR" serve-peer perf
# Received source UUIDs must also work when the replica sends new states back.
AR=$LAYR_ROOT
return_from_replica() (
    export LAYR_ROOT=$BR
    l --project perf remote add test-peer ssh://test/perf
    l --project perf adopt main >/dev/null
    cd "$BR/projects/perf/lines/main" || exit 1
    rm blob
    l save -qam replica-remove
    cp --reflink=always "$BR/projects/perf/states/$first/blob" blob
    l add blob
    l save -qm replica-return
    head_id > "$T/return-head"
    peer_exchange push perf -q -- env LAYR_ROOT="$AR" "$LAYR" serve-peer perf
) >"$T/return.log" 2>&1
ok "replica reuses received sources when it sends back" return_from_replica
sed 's/^/  /' "$T/return.log"
returned=$(cat "$T/return-head")
ok "the original machine receives the same blob" cmp blob "$P/states/$returned/blob"
btrfs filesystem sync "$LAYR_ROOT" >/dev/null
read -r _total exclusive _shared _name < <(btrfs filesystem du --raw "$P/states/$returned/blob" | tail -1)
if btrfs version | python3 -c 'import re,sys; m=re.search(r"v(\d+)\.(\d+)",sys.stdin.read()); sys.exit(not (m and tuple(map(int,m.groups())) >= (6,11)))'; then
    eq "the return trip also shares the blob extents" 0 "$exclusive"
else
    eq "older receive tools use a separate exact copy on return" 16777216 "$exclusive"
fi
ok "replica can send to another receiver" env LAYR_ROOT="$BR" "$LAYR" --project perf push test-peer --command -q -- env LAYR_ROOT="$CR" "$LAYR" serve-peer perf
ok "the third machine receives the same blob" cmp blob "$CR/projects/perf/states/$returned/blob"
finish
