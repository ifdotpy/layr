#!/bin/bash
# ci: macos overlayfs
# Busy cache folders must stay out of automatic source states and the base object store.
. "$(dirname "$0")/lib.sh"
export LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock LAYR_TEST_PERIODIC=1
mkdir -p "$T/src"
echo source > "$T/src/file.txt"
echo target/ > "$T/src/.gitignore"
python3 - "$T" "$LAYR" <<'PY'
import errno
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

root, binary = Path(sys.argv[1]), sys.argv[2]
project = Path(os.environ['LAYR_ROOT']) / 'projects/p'
work = project / 'lines/main'

def run(*args, cwd=None):
    result = subprocess.run([binary, *args], cwd=cwd or work, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0 and 'periodic save (' not in result.stderr, (args, result.stdout, result.stderr)
    if result.stderr:
        print(result.stderr, file=sys.stderr)
    return result.stdout

def retire():
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True)
    run('--project', 'p', 'line', 'list', cwd=root)

run('init', 'p', '--from', str(root / 'src'), '-q', cwd=root)
run('protect', 'main', '--remove')
run('line', 'new', 'build', 'main')
run('config', 'regenerable', '["target"]')
run('overlay', 'create', 'build-cache', '--folder', 'target', '--quiet', '--history', 'last', '-q')
hidden = []
try:
    for name in ('main', 'build'):
        line = project / 'lines' / name
        try:
            line.rename(root / f'hidden-{name}')
            hidden.append(name)
        except OSError as error:
            if error.errno not in (errno.EXDEV, errno.EBUSY):
                raise
    run('--project', 'p', 'migrate', cwd=root)
    retire()
    run('--project', 'p', 'migrate', '--delete-old', cwd=root)
finally:
    for name in hidden:
        (root / f'hidden-{name}').rename(project / 'lines' / name)
work = project / 'lines/build'
retire()
run('config', 'snapshot.interval', '0')
(work / 'target').mkdir(exist_ok=True)
cache = work / 'target/build-output'
# Holding a writable descriptor makes cache conversion refuse the folder, as a build does.
with cache.open('wb') as writer:
    for revision in range(3):
        payload = b'\x7fELF' + bytes([revision + 1]) * 8192
        writer.seek(0)
        writer.write(payload)
        writer.flush()
        if revision < 2:
            (work / 'file.txt').write_text(f'source revision {revision}\n')
        run('__test-periodic', os.environ['LAYR_ROOT'], cwd=root)
        blobs = project / 'meta/objects/blobs'
        assert not any(p.read_bytes() == payload for p in blobs.glob('*/*') if p.is_file()), 'cache binary leaked into the base store'
        with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
            states = db.execute("SELECT id FROM states WHERE kind='auto' AND json_extract(rec, '$.tree') IS NOT NULL").fetchall()
        assert states, 'periodic save wrote an automatic object state'
        for (state,) in states:
            checkout = root / f'checkout-{revision}-{state}'
            run('objects', 'checkout', state, str(checkout))
            assert not (checkout / 'target/build-output').exists(), 'automatic source state contains cache output'
        retire()
# Once the writer closes, the next pass converts and saves the cache in its own store.
(work / 'file.txt').write_text('build finished\n')
run('__test-periodic', os.environ['LAYR_ROOT'], cwd=root)
overlays = list((project / 'meta/overlays').glob('*/meta/objects/blobs/*/*'))
assert any(p.is_file() and p.read_bytes() == payload for p in overlays), 'cache binary is saved in the overlay store'
assert cache.read_bytes() == payload, 'the working build output stays intact'
print('busy periodic saves exclude all binary revisions; completed build is in the overlay store')
PY
eq "cache parts stay in their own object store" "0" "$?"
finish
