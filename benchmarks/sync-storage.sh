#!/bin/bash
# Compare actual sync streams and received data extents. Run as root on Linux.
# Usage: bash benchmarks/sync-storage.sh BEFORE AFTER OUTPUT.json
set -e
BEFORE=$(realpath "$1")
export LAYR=$(realpath "$2")
OUTPUT=$(realpath -m "$3")
PROJECT=$(cd "$(dirname "$0")/.." && pwd)
. "$PROJECT/tests/lib.sh"
mkdir "$T/src"
echo initial > "$T/src/f"
l init perf --from "$T/src" -q
P=$LAYR_ROOT/projects/perf
head_id() { l rev-parse HEAD | python3 -c 'import sys, uuid; s=sys.stdin.read().strip(); print(uuid.UUID(hex=s[20:]+s[12:20]+s[:12]))'; }
cd "$P/lines/main"
python3 - <<'PY'
import random
r = random.Random(42)
with open('blob', 'wb') as f:
    for _ in range(128):
        f.write(r.randbytes(128 * 1024))
PY
l add blob
l commit -qm blob
first=$(head_id)
rm blob
l commit -qam remove
cp --reflink=always "$P/states/$first/blob" blob
l add blob
l commit -qm return
last=$(head_id)
new_fs "$T/before" 2G
new_fs "$T/after" 2G
for name in before after; do
    binary=$BEFORE
    [ "$name" = before ] || binary=$LAYR
    "$binary" sync -q -- bash -c "tee '$T/$name-clone.wire' | env LAYR_ROOT='$T/$name/store' '$binary' serve-peer perf"
    cmp blob "$T/$name/store/projects/perf/states/$last/blob"
    btrfs filesystem sync "$T/$name" >/dev/null
    btrfs filesystem du --raw "$T/$name/store/projects/perf/states/$last/blob" > "$T/$name.du"
done
btrfs property set . compression zstd
python3 - <<'PY'
with open('compressed', 'wb') as f:
    for _ in range(128):
        f.write(b'source text\n' * 12000)
PY
l add compressed
l commit -qm compressed
last=$(head_id)
for name in before after; do
    binary=$BEFORE
    [ "$name" = before ] || binary=$LAYR
    "$binary" sync -q -- bash -c "tee '$T/$name-compression.wire' | env LAYR_ROOT='$T/$name/store' '$binary' serve-peer perf"
    cmp compressed "$T/$name/store/projects/perf/states/$last/compressed"
done
python3 - "$T" "$BEFORE" "$LAYR" "$OUTPUT" <<'PY'
import collections, hashlib, json, pathlib, struct, sys
root = pathlib.Path(sys.argv[1])
result = {'fixture': {'blob_bytes': 16 << 20, 'compressed_file_bytes': 128 * 12000 * 12}, 'binaries': {}, 'clone': {}, 'compression': {}}
for name, binary in zip(['before', 'after'], sys.argv[2:4]):
    result['binaries'][name] = {'path': binary, 'sha256': hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest()}
    for fixture in ['clone', 'compression']:
        wire = (root / f'{name}-{fixture}.wire').read_bytes()
        pos, payload = 0, 0
        stream = bytearray()
        counts = collections.Counter()
        versions = set()
        while pos < len(wire):
            kind = wire[pos:pos+1]
            size = struct.unpack_from('>I', wire, pos + 1)[0]
            data = wire[pos+5:pos+5+size]
            pos += 5 + size
            if kind == b'D':
                payload += size
                stream.extend(data)
            elif kind == b'E':
                assert stream[:13] == b'btrfs-stream\0'
                versions.add(struct.unpack_from('<I', stream, 13)[0])
                i = 17
                while i < len(stream):
                    n, cmd = struct.unpack_from('<IH', stream, i)
                    counts[cmd] += 1
                    i += 10 + n
                assert i == len(stream)
                stream.clear()
        row = {'wire_bytes': len(wire), 'stream_bytes': payload, 'versions': sorted(versions), 'commands': dict(counts)}
        if fixture == 'clone':
            total, exclusive, shared, *_ = (root / f'{name}.du').read_text().splitlines()[-1].split()
            row['restored_blob_extents'] = {'total': int(total), 'exclusive': int(exclusive), 'shared': int(shared)}
        result[fixture][name] = row
assert result['clone']['after']['stream_bytes'] < result['clone']['before']['stream_bytes']
assert result['clone']['before']['restored_blob_extents']['total'] == 16 << 20
assert result['clone']['before']['restored_blob_extents']['exclusive'] == 16 << 20
assert result['clone']['after']['restored_blob_extents']['total'] == 16 << 20
assert result['clone']['after']['restored_blob_extents']['exclusive'] == 0
assert result['compression']['after']['stream_bytes'] < result['compression']['before']['stream_bytes']
assert result['compression']['after']['commands'].get(25, 0) > 0
pathlib.Path(sys.argv[4]).write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result))
PY
