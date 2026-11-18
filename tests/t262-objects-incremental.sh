#!/bin/bash
# ci: macos overlayfs
# Saves use checked object deltas after the native parent has been retired.
. "$(dirname "$0")/lib.sh"
export LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock
mkdir -p "$T/src/sub"
python3 - "$T" "$LAYR" <<'PY'
import errno
import json
import os
import re
from pathlib import Path
import sqlite3
import stat
import subprocess
import sys

root, binary = Path(sys.argv[1]), sys.argv[2]
project = Path(os.environ['LAYR_ROOT']) / 'projects/p'
work = project / 'lines/main'
for i in range(100):
    (root / f'src/sub/f{i}').write_text(f'original {i}\n')

def run(*args, cwd=None, trace=False):
    env = dict(os.environ, **({'LAYR_TRACE': '1'} if trace else {}))
    result = subprocess.run([binary, *args], cwd=cwd or root, env=env, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result

def retire():
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        # Unprivileged btrfs fixture cleanup, as in t255-periodic-yields.sh.
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True, timeout=3)
    run('line', 'list', cwd=work if work.exists() else root)

def head_refs():
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        head = db.execute("SELECT head FROM lines WHERE name='main'").fetchone()[0]
        state = json.loads(db.execute('SELECT rec FROM states WHERE id=?', (head,)).fetchone()[0])
    assert state['tree'] and state['times']
    # Off macOS the head of a line with a working folder keeps its native copy for the next
    # incremental write (t257-btrfs-incremental-write.sh); elsewhere it must be retired.
    if sys.platform == 'darwin':
        assert not any((project / kind / head).exists() for kind in ('states', 'deltas')), 'native parent must be retired'
    return state['tree'], state['times']

run('init', 'p', '--no-objects', '--from', str(root / 'src'), '-q')
run('protect', 'main', '--remove', cwd=work)
# Hide the folder during migration when possible, avoiding unprivileged btrfs send.
try:
    work.rename(root / 'hidden')
    hidden = True
except OSError as e:
    if e.errno not in (errno.EXDEV, errno.EBUSY):
        raise
    hidden = False
try:
    run('--project', 'p', 'migrate')
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True, timeout=3)
    run('--project', 'p', 'migrate', '--delete-old')
finally:
    if hidden:
        (root / 'hidden').rename(work)
retire()
head_refs()
for i in range(5):
    (work / f'sub/f{i}').write_text(f'changed {i}\n')
run('add', '-A', cwd=work)
retire()
head_refs()
def incremental(trace, files):
    modes = re.findall(r'objects write Incremental \{ changed_paths: (\d+) \}: (\d+) files', trace)
    assert len(modes) == 1, trace
    paths, imported = map(int, modes[0])
    # APFS thin deltas also list the containing directory. Count imported files
    # independently from paths, which may include directories and removals.
    assert imported == files and paths >= files, trace

def manifest(folder):
    entries = {}
    for path in [folder, *folder.rglob('*')]:
        rel = str(path.relative_to(folder))
        meta = path.lstat()
        # The tree format stores entry modes; the root has only its own times table.
        mode = stat.S_IMODE(meta.st_mode) if path != folder else 0
        data = path.read_bytes() if stat.S_ISREG(meta.st_mode) else None
        entries[rel] = (stat.S_IFMT(meta.st_mode), mode, meta.st_mtime_ns, data)
    return entries

def preserve_stage(name):
    # This fixture only edits existing files. Overlay the native stage's entries
    # on the previous head BEFORE publication, preserving every byte, mode and ns.
    baseline = root / f'{name}-parent'
    run('objects', 'checkout', 'head', str(baseline), cwd=work)
    expected = manifest(baseline)
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        line = db.execute("SELECT id FROM lines WHERE name='main'").fetchone()[0]
    expected.update(manifest(project / 'meta/stage' / line))
    head_refs()  # Preparing the independent reference must not retain the parent.
    return expected

def verify_head(expected, name):
    retire()
    head_refs()
    checkout = root / f'{name}-saved'
    run('objects', 'checkout', 'head', str(checkout), cwd=work)
    actual = manifest(checkout)
    differences = [path for path in expected.keys() | actual.keys() if expected.get(path) != actual.get(path)]
    assert not differences, differences

expected = preserve_stage('default')
result = run('save', '-qm', 'Five changed files', cwd=work, trace=True)
incremental(result.stderr, 5)
verify_head(expected, 'default')
for i in range(5):
    assert run('show', f'head:sub/f{i}', cwd=work).stdout == f'changed {i}\n'
assert run('show', 'head:sub/f99', cwd=work).stdout == 'original 99\n'
# A restore of an unchanged staged path requests a complete index on APFS too.
# This case must carry the existing comparison into publication on every backend.
for i in range(5):
    (work / f'sub/f{i}').write_text(f'checked {i}\n')
run('add', '-A', cwd=work)
run('restore', '--staged', 'sub/f99', cwd=work)
retire()
head_refs()
expected = preserve_stage('complete')
result = run('save', '-qm', 'Reuse the complete-snapshot comparison', cwd=work, trace=True)
incremental(result.stderr, 5)
assert 'write checked objects' in result.stderr, result.stderr
verify_head(expected, 'complete')
# Automatic saves also remain incremental; APFS can publish a thin layer here.
retire()
(work / 'sub/f9').write_text('automatic change\n')
run('config', 'snapshot.interval', '0', cwd=work)
env = dict(os.environ, LAYR_TEST_PERIODIC='1', LAYR_TRACE='1')
result = subprocess.run([binary, '__test-periodic', os.environ['LAYR_ROOT']], env=env,
                        capture_output=True, text=True, timeout=30)
assert result.returncode == 0, result.stderr
incremental(result.stderr, 1)
# Read records directly: a CLI command here could retire the first automatic save.
with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
    records = [json.loads(row[0]) for row in db.execute('SELECT rec FROM states')]
first_auto, = [state for state in records if state.get('message') == 'periodic']
if sys.platform == 'darwin':
    assert (project / 'deltas' / first_auto['id']).is_dir(), 'first automatic layer must remain native'
(work / 'sub/f9').write_text('second automatic change\n')
result = subprocess.run([binary, '__test-periodic', os.environ['LAYR_ROOT']], env=env,
                        capture_output=True, text=True, timeout=30)
assert result.returncode == 0, result.stderr
incremental(result.stderr, 1)
if sys.platform == 'darwin':
    # The imported count alone hides a full comparison that hashes unchanged files.
    assert 'objects full comparison' not in result.stderr, result.stderr
with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
    records = [json.loads(row[0]) for row in db.execute('SELECT rec FROM states')]
second_auto, = [state for state in records
                if state.get('message') == 'periodic' and state['id'] != first_auto['id']]
assert second_auto['parents'] == first_auto['parents']
if sys.platform == 'darwin':
    # Python has no os.getxattr on macOS.
    marker = json.loads(subprocess.check_output(
        ['xattr', '-p', 'com.layr.subvolume', str(project / 'deltas' / second_auto['id'])], timeout=30))
    assert marker['base'] == first_auto['id'], 'second layer must use the unretired automatic base'
checkout = root / 'second-automatic-saved'
run('objects', 'checkout', second_auto['id'], str(checkout), cwd=work)
assert (checkout / 'sub/f8').read_text() == 'original 8\n'
assert (checkout / 'sub/f9').read_text() == 'second automatic change\n'
assert (checkout / 'sub/f99').read_text() == 'original 99\n'
print('default and complete-stage named saves and repeated automatic saves remain incremental')
PY
eq "retired object parents keep incremental saves" 0 "$?"
finish
