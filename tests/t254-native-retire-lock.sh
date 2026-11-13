#!/bin/bash
# ci: macos overlayfs
# Issue #442: a finished command must not wait for the project lock in native-copy cleanup.
# A migrated native state is queued exactly as objstore::queue_native queues a retry.
. "$(dirname "$0")/lib.sh"
export LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock

mkdir -p "$T/src"
echo content > "$T/src/file.txt"
python3 - "$T" "$LAYR" <<'PY'
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary = Path(sys.argv[1]), sys.argv[2]
project = Path(os.environ['LAYR_ROOT']) / 'projects/p'
work = project / 'lines/main'
meta = project / 'meta'

def run(*args, cwd=None):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f'{args}: {result.returncode}\n{result.stdout}{result.stderr}')
    return result.stdout

def until(predicate, description):
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise RuntimeError(f'timed out: {description}')

run('init', 'p', '--from', str(root / 'src'), '-q')
# Direct non-root commands still enforce main's default protection.
run('protect', 'main', '--remove', cwd=work)
# This fixture has no unsaved work. Hide its folder during migration to avoid the
# automatic-save comparison, whose btrfs metadata send requires root.
work.rename(root / 'main')
try:
    run('--project', 'p', 'migrate', cwd=root)
finally:
    (root / 'main').rename(work)
with sqlite3.connect(meta / 'db/layr.sqlite') as db:
    state = db.execute("SELECT head FROM lines WHERE name='main'").fetchone()[0]
    record = json.loads(db.execute('SELECT rec FROM states WHERE id=?', (state,)).fetchone()[0])
    assert record.get('tree') and record.get('times'), 'the native state has durable objects'
native = next(p for p in (project / 'states' / state, project / 'deltas' / state) if p.is_dir())
if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
    # Unprivileged btrfs deletion cannot retire a sealed native copy on this mount.
    subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'],
                   check=True, timeout=3)
# Migration without --delete-old retains this native copy. Model a pending retry using
# queue_native's empty marker, so the fixture needs no race with publication/retirement.
queue = meta / 'object-native'
queue.mkdir(exist_ok=True)
marker = queue / state
marker.touch()

holder = reader = None
prompt = False
try:
    with (root / 'holder.log').open('w') as log:
        holder = subprocess.Popen([binary, 'tag', 'held', 'head'], cwd=work,
                                  env=dict(os.environ, LAYR_TEST_LOCK_HOLD_MS='5000'),
                                  stdout=log, stderr=log)

    def locked():
        try:
            return json.loads((meta / 'lock.holder').read_text())['pid'] == holder.pid
        except (FileNotFoundError, json.JSONDecodeError):
            return False

    until(locked, 'the holder acquired the project lock')
    assert marker.exists(), 'native retirement is pending while the lock is held'
    started = time.monotonic()
    with (root / 'reader.out').open('w') as out, (root / 'reader.err').open('w') as err:
        reader = subprocess.Popen([binary, 'line', 'list'], cwd=work, stdout=out, stderr=err)
    # run_as flushes command output before free::finish, proving the command itself finished.
    until(lambda: 'main' in (root / 'reader.out').read_text(), 'line list completed its work')
    work_elapsed = time.monotonic() - started
    try:
        reader.wait(timeout=1)
        prompt = reader.returncode == 0
    except subprocess.TimeoutExpired:
        pass
    elapsed = time.monotonic() - started
    print(f'line list output: {work_elapsed:.3f}s; return: {elapsed:.3f}s; '
          f'holder delay: 5.000s; exited: {reader.poll() is not None}', flush=True)
    assert holder.poll() is None and locked(), 'the holder still owns the project lock'
    assert marker.exists(), 'busy cleanup leaves the native copy queued for retry'
finally:
    # Stop the sleeping holder before its commit; never leave it or a blocked reader behind.
    if holder is not None and holder.poll() is None:
        holder.kill()
        holder.wait(timeout=3)
    if reader is not None and reader.poll() is None:
        try:
            reader.wait(timeout=10)
        except subprocess.TimeoutExpired:
            reader.kill()
            reader.wait(timeout=3)
            raise

assert reader.returncode == 0, (root / 'reader.err').read_text()
gc_output = run('gc', '-q', cwd=work)
assert not marker.exists(), ('a later gc retires the queued native copy',
                             (root / 'reader.err').read_text(), gc_output)
print('the reader and a later gc finish after the holder releases the lock', flush=True)
sys.exit(0 if prompt else 1)
PY
eq "finished commands return within one second while native retirement waits for the lock" 0 "$?"
finish
