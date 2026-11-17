#!/bin/bash
# ci: macos overlayfs
# Object maintenance shares its plan with gc -n, yields to locks and compresses outside them.
. "$(dirname "$0")/lib.sh"
export LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock LAYR_TEST_PERIODIC=1
mkdir -p "$T/src"
echo initial > "$T/src/file.txt"
python3 - "$T" "$LAYR" <<'PY'
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary = Path(sys.argv[1]), sys.argv[2]
project = Path(os.environ['LAYR_ROOT']) / 'projects/p'
work, meta = project / 'lines/main', project / 'meta'
objects = meta / 'objects'
stamp = meta / 'objects-maintained'

def run(*args, cwd=None, env=None, timeout=15):
    r = subprocess.run([binary, *args], cwd=cwd or work, env=env, capture_output=True, text=True, timeout=timeout)
    assert r.returncode == 0, (args, r.stdout, r.stderr)
    return r.stdout

def periodic(**kw):
    return run('__test-periodic', os.environ['LAYR_ROOT'], cwd=root, **kw)

def inventory():
    return {str(p.relative_to(objects)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in objects.rglob('*') if p.is_file()}

run('init', 'p', '--objects', '--from', str(root / 'src'), '-q', cwd=root)
run('protect', 'main', '--remove')
# Use object-backed comparisons even on an unprivileged btrfs mount (as in t255).
try:
    work.rename(root / 'main')
    hidden = True
except OSError as e:
    if e.errno not in (errno.EXDEV, errno.EBUSY):
        raise
    hidden = False
try:
    run('--project', 'p', 'migrate', cwd=root)
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True)
    run('--project', 'p', 'migrate', '--delete-old', cwd=root)
finally:
    if hidden:
        (root / 'main').rename(work)
run('config', 'snapshot.interval', '0')
run('config', 'objects.maintenance-interval', '0')
run('config', 'objects.hot-states', '1')
autos = []
for n in range(3):
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True)
    run('line', 'list')  # Retire the preceding automatic state's disposable native copy.
    (work / 'file.txt').write_text(f'version {n}\n')
    periodic()
    with sqlite3.connect(meta / 'db/layr.sqlite') as db:
        autos.append(db.execute("SELECT id FROM states WHERE kind='auto' ORDER BY time DESC LIMIT 1").fetchone()[0])
run('config', 'snapshot.interval', '3600')
with sqlite3.connect(meta / 'db/layr.sqlite') as db:
    head = db.execute("SELECT head FROM lines WHERE name='main'").fetchone()[0]
# A state mapping and a hot blob no database owner reaches must be pruned, not cooled.
(objects / 'states/orphan').write_bytes((objects / 'states' / head).read_bytes())
dead = b'unreachable hot blob'
digest = hashlib.sha256(f'blob {len(dead)}\0'.encode() + dead).hexdigest()
dead_path = objects / 'blobs' / digest[:2] / digest[2:]
dead_path.parent.mkdir(exist_ok=True)
dead_path.write_bytes(dead)
before = inventory()
preview = run('gc', '-n')
assert 'would remove 1 state record(s)' in preview, preview
assert '0 object(s), 1 hot blob(s)' in preview, preview
assert 'would turn cold 3 hot blob(s)' in preview, preview
assert 'would pack' in preview and 'source bytes' in preview, preview
assert inventory() == before, 'gc -n must not change object files'
assert not stamp.exists(), 'preview does not postpone periodic work'
# Disabled maintenance changes neither data nor timestamp.
run('config', 'objects.maintenance-interval', '0')
periodic()
assert inventory() == before and not stamp.exists()
run('config', 'objects.maintenance-interval', '60')
# Both locks are nonblocking, and skipped work remains due.
for name in ('lock', 'objects-maintenance.lock'):
    with (meta / name).open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        periodic(timeout=3)
        assert inventory() == before and not stamp.exists(), name
barrier = root / 'repack-barrier'
barrier.mkdir()
child = None
try:
    child = subprocess.Popen([binary, '__test-periodic', os.environ['LAYR_ROOT']],
        cwd=root, env=dict(os.environ, LAYR_TEST_OBJECTS_REPACK_BARRIER=str(barrier)),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    deadline = time.monotonic() + 4
    while not (barrier / 'ready').exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert (barrier / 'ready').exists(), 'maintenance reached compression'
    with (meta / 'lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert child.poll() is None, 'the compressor is still waiting'
        (barrier / 'release').touch()
        stdout, stderr = child.communicate(timeout=15)
        assert child.returncode == 0, (stdout, stderr)
        assert stamp.exists(), stderr
finally:
    if child is not None and child.poll() is None:
        child.kill()
        child.wait()
after = json.loads(run('objects', 'stat', '--json'))
assert after['hot_blobs'] == 1, after
assert after['packs'] > 0 and after['packed_objects'] > 0, after
assert not dead_path.exists() and not (objects / 'states/orphan').exists()
preview = run('gc', '-n')
assert 'would remove 0 state record(s), 0 object(s), 0 hot blob(s), 0 bytes' in preview, preview
assert 'would turn cold 0 hot blob(s)' in preview, preview
unchanged = inventory()
periodic()
assert inventory() == unchanged, 'the interval throttles the next pass'
# A manual run follows the same plan, and finishes the cold encodings in this run.
preview = next(line for line in run('gc', '-n').splitlines() if line.startswith('objects: base:'))
if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
    for native in (project / 'states').iterdir():
        subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True)
actual = next(line for line in run('gc').splitlines() if line.startswith('objects: base:'))
assert actual == preview.replace('would remove', 'removed').replace('would turn cold', 'turned cold').replace('would pack', 'packed'), (preview, actual)
assert json.loads(run('objects', 'stat', '--json'))['loose_bytes'] == 0
# A following batch packs the newly cold encodings, and the old state still checks out.
stamp.unlink()
periodic()
run('objects', 'checkout', autos[1], str(root / 'restored'))
assert (root / 'restored/file.txt').read_text() == 'version 1\n'
# An import holds its store lease from the first object until publication. Periodic prune
# must skip a store whose state mapping does not yet name its newly written objects.
import_src = root / 'import-src'
import_src.mkdir()
for n in range(20):
    (import_src / f'file{n}').write_text(f'imported {n}\n')
run('init', 'q', '--no-objects', '--from', str(import_src), '-q', cwd=root)
q = Path(os.environ['LAYR_ROOT']) / 'projects/q'
run('config', 'snapshot.interval', '3600', cwd=q / 'lines/main')
import_barrier = root / 'import-barrier'
import_barrier.mkdir()
child = None
try:
    child = subprocess.Popen([binary, '--project', 'q', '--line', 'main', 'objects', 'import', 'head', '--full'],
        cwd=root, env=dict(os.environ, LAYR_TEST_OBJECTS_IMPORT_BARRIER=str(import_barrier)),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    deadline = time.monotonic() + 4
    while not (import_barrier / 'ready').exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert (import_barrier / 'ready').exists(), ('import has written objects', child.communicate(timeout=1) if child.poll() is not None else 'still running')
    qobjects = q / 'meta/objects'
    unpublished = {p: p.read_bytes() for p in qobjects.rglob('*') if p.is_file()}
    assert len(list((qobjects / 'blobs').glob('*/*'))) == 20
    assert not list((qobjects / 'states').iterdir()), 'no root has been published yet'
    periodic()
    assert child.poll() is None
    assert all(p.exists() and p.read_bytes() == data for p, data in unpublished.items())
    assert not (q / 'meta/objects-maintained').exists(), 'busy imports remain due'
    (import_barrier / 'release').touch()
    stdout, stderr = child.communicate(timeout=15)
    assert child.returncode == 0, (stdout, stderr)
finally:
    if child is not None and child.poll() is None:
        child.kill()
        child.wait()
run('--project', 'q', '--line', 'main', 'objects', 'checkout', 'head', str(root / 'imported'), cwd=root)
for n in range(20):
    assert (root / 'imported' / f'file{n}').read_text() == f'imported {n}\n'
# Pause readers immediately before their hot-file open, then cool that file. Both checkout
# and direct blob reads must use the cold representation when the open returns ENOENT.
for kind in ('checkout', 'read'):
    data = b'version 1\n'
    digest = hashlib.sha256(f'blob {len(data)}\0'.encode() + data).hexdigest()
    hot = objects / 'blobs' / digest[:2] / digest[2:]
    hot.parent.mkdir(exist_ok=True)
    hot.write_bytes(data)
    barrier = root / f'hot-open-{kind}'
    barrier.mkdir()
    args = ['objects', 'checkout', autos[1], str(root / 'race-restored')] if kind == 'checkout' else ['show', f'{autos[1]}:file.txt']
    child = None
    try:
        child = subprocess.Popen([binary, *args], cwd=work,
            env=dict(os.environ, LAYR_TEST_HOT_OPEN_BARRIER=str(barrier)),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + 4
        while not (barrier / 'ready').exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        assert (barrier / 'ready').exists(), f'{kind} reached hot open'
        run('objects', 'repack')
        assert not hot.exists(), 'cooling unlinked the hot path'
        (barrier / 'release').touch()
        stdout, stderr = child.communicate(timeout=15)
        assert child.returncode == 0, (kind, stdout, stderr)
        if kind == 'checkout':
            assert (root / 'race-restored/file.txt').read_bytes() == data
        else:
            assert stdout == data.decode(), (stdout, stderr)
    finally:
        if child is not None and child.poll() is None:
            child.kill()
            child.wait()
# Overlay stores live below their parent's metadata, outside Store::project_names.
run('overlay', 'create', 'notes', '-q')
overlay_objects = project / 'meta/overlays/notes/meta/objects'
assert overlay_objects.is_dir()
orphan = b'overlay garbage'
digest = hashlib.sha256(f'blob {len(orphan)}\0'.encode() + orphan).hexdigest()
orphan_path = overlay_objects / 'blobs' / digest[:2] / digest[2:]
orphan_path.parent.mkdir(exist_ok=True)
orphan_path.write_bytes(orphan)
overlay_meta = overlay_objects.parent
# APFS project opening must defer pending recovery before reaching the maintenance locks.
# The same held-lock case checks ordinary overlay maintenance on Linux.
pending = overlay_meta / 'pending/periodic-test'
with (overlay_meta / 'lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    if sys.platform == 'darwin':
        pending.mkdir(parents=True)
    try:
        periodic(timeout=3)
        assert orphan_path.exists(), 'busy overlay work remains due'
        assert not (overlay_meta / 'objects-maintained').exists()
    finally:
        if sys.platform == 'darwin':
            pending.rmdir()
periodic()
assert not orphan_path.exists(), 'periodic maintenance discovers nested overlay stores'
assert (project / 'meta/overlays/notes/meta/objects-maintained').exists()
# Metadata for another platform's overlay can arrive without its local project. On OverlayFS the store's
# registry mounts the database of every overlay it made, so a removed folder is no such case there.
if not os.environ.get('LAYR_OVERLAYFS_TEST_ROOT'):
    run('overlay', 'create', 'far', '-q')
    try:
        (project / 'meta/overlays/far').rename(root / 'far-not-fetched')
    except OSError as error:
        if error.errno != errno.EXDEV:
            raise
        # On btrfs the store is its own file system: keep the folder on it, outside the project.
        (project / 'meta/overlays/far').rename(Path(os.environ['LAYR_ROOT']) / 'far-not-fetched')
run('gc', '-n')
run('gc')
print('periodic objects: dry-run parity, pruning, cooling, packing, interval and lock yielding passed')
PY
eq "periodic object maintenance and gc preview" 0 "$?"
finish
