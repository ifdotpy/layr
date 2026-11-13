#!/bin/bash
# ci: macos overlayfs
# Issue #442: a periodic pass skips busy locks without marking the line checked, then retries.
. "$(dirname "$0")/lib.sh"
export LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock LAYR_TEST_PERIODIC=1
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
work, meta = project / 'lines/main', project / 'meta'

def run(*args, cwd=None, env=None, timeout=10):
    result = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result

def until(predicate, description):
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError(description)

run('init', 'p', '--from', str(root / 'src'), '-q')
run('protect', 'main', '--remove', cwd=work)
# Object-backed comparisons also work on an unprivileged btrfs test mount.
work.rename(root / 'main')
try:
    run('--project', 'p', 'migrate', cwd=root)
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        # Unprivileged btrfs deletion requires writable test snapshots on this mount.
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True, timeout=3)
    run('--project', 'p', 'migrate', '--delete-old', cwd=root)
finally:
    (root / 'main').rename(work)
run('config', 'snapshot.interval', '600', cwd=work)
with sqlite3.connect(meta / 'db/layr.sqlite') as db:
    line = db.execute("SELECT id FROM lines WHERE name='main'").fetchone()[0]

def autos():
    with sqlite3.connect(meta / 'db/layr.sqlite') as db:
        return db.execute("SELECT count(*) FROM states WHERE kind='auto' AND line=?", (line,)).fetchone()[0]

def checks():
    path = meta / 'periodic-checks.json'
    return json.loads(path.read_text()) if path.exists() else {}

for kind in ('line', 'project'):
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True, timeout=3)
    run('line', 'list', cwd=work)  # Retire the preceding pass's native copy.
    before = autos()
    (meta / 'periodic-checks.json').unlink(missing_ok=True)
    (work / 'file.txt').write_text(f'changed with busy {kind}\n')
    log_path = root / f'{kind}.log'
    env = dict(os.environ, LAYR_TRACE='1')
    if kind == 'line':
        args = ['merge', '-q', 'main']
        env['LAYR_TEST_LINE_LOCK_MS'] = '5000'
    else:
        args = ['tag', 'held', 'head']
        env['LAYR_TEST_LOCK_HOLD_MS'] = '5000'
    holder = periodic = None
    try:
        with log_path.open('w') as log:
            holder = subprocess.Popen([binary, *args], cwd=work, env=env, stdout=log, stderr=log)
        def locked():
            if kind == 'line':
                return 'line lock taken: main' in log_path.read_text()
            try:
                return json.loads((meta / 'lock.holder').read_text())['pid'] == holder.pid
            except (FileNotFoundError, json.JSONDecodeError):
                return False
        until(locked, f'{kind} lock acquired')
        started = time.monotonic()
        result = run('__test-periodic', os.environ['LAYR_ROOT'], timeout=1)
        elapsed = time.monotonic() - started
        assert holder.poll() is None and locked(), 'the user still holds the lock'
        assert line not in checks(), 'a busy line was not marked checked'
        assert autos() == before, 'a busy line was not saved'
        print(f'busy {kind}: periodic returned in {elapsed:.3f}s', flush=True)
    finally:
        if holder is not None and holder.poll() is None:
            holder.kill()
            holder.wait(timeout=3)
    # The failed pass did not advance the check time. The next pass must compare and save.
    barrier = root / f'{kind}-barrier'
    barrier.mkdir()
    try:
        with (root / f'{kind}-retry.log').open('w') as log:
            periodic = subprocess.Popen([binary, '__test-periodic', os.environ['LAYR_ROOT']],
                                        env=dict(os.environ, LAYR_TEST_PERIODIC_SAVE_BARRIER=str(barrier)),
                                        stdout=log, stderr=log)
        until(lambda: (barrier / 'ready').exists(), 'the next pass retries the save')
        assert periodic.poll() is None, 'save waits at the barrier'
        (barrier / 'release').touch()
        periodic.wait(timeout=8)
        assert periodic.returncode == 0
        assert autos() == before + 1, (root / f'{kind}-retry.log').read_text()
        assert line not in checks(), 'a saved state uses its own timestamp'
        print(f'busy {kind}: next pass saved the change', flush=True)
    finally:
        if periodic is not None and periodic.poll() is None:
            periodic.kill()
            periodic.wait(timeout=3)
    # Make this line due again for the second case without sleeping for the interval.
    with sqlite3.connect(meta / 'db/layr.sqlite') as db:
        db.execute("UPDATE states SET time=1 WHERE kind='auto' AND line=?", (line,))
PY
eq "periodic saves yield to user locks and retry skipped lines" 0 "$?"
finish
