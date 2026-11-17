#!/bin/bash
# ci: macos overlayfs
# File history keeps reading portable states after their native copies are retired.
. "$(dirname "$0")/lib.sh"

python3 - "$T" "$LAYR" <<'PY'
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

root, binary = Path(sys.argv[1]), sys.argv[2]
source = root / 'source'
source.mkdir()
(source / 'file.txt').write_text('initial\n')

def run(*args, cwd=root):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout

run('init', 'history', '--objects', '--from', str(source), '-q')
project = Path(os.environ['LAYR_ROOT']) / 'projects/history'
work = project / 'lines/main'
run('protect', 'main', '--remove', cwd=work)

def retire():
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        # Let uid 1000 remove its own sealed btrfs fixture snapshots, as in t262.
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True, timeout=3)
    run('line', 'list', cwd=work)
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        head = db.execute("SELECT head FROM lines WHERE name='main'").fetchone()[0]
        parents = json.loads(db.execute('SELECT rec FROM states WHERE id=?', (head,)).fetchone()[0]).get('parents') or []
    # Off macOS the head of a line with a working folder keeps its native copy for the next
    # incremental write (t257-btrfs-incremental-write.sh): its parents must be object-only.
    retired = [head] if sys.platform == 'darwin' else parents
    assert not any((project / kind / s).exists() for s in retired for kind in ('states', 'deltas')), 'fixture must be object-only'

retire()
initial = run('show', '-s', '--format=%s', 'head', cwd=work).strip()
assert run('log', '--format=%s', '--', 'file.txt', cwd=work).strip() == initial
(work / 'file.txt').write_text('first change\n')
run('save', '-qam', 'First change', cwd=work)
retire()
assert run('log', '-1', '--format=%s', '--', 'file.txt', cwd=work).strip() == 'First change'
assert run('log', '-1', '--format=%s', '--', '*.txt', cwd=work).strip() == 'First change'
assert run('log', '--format=%s', '--', 'absent.txt', cwd=work) == ''
assert 'file.txt' in run('log', '-1', '--stat', '--', 'file.txt', cwd=work)
assert '+first change' in run('log', '-1', '-p', '--', 'file.txt', cwd=work)
(work / 'file.txt').rename(work / 'renamed.txt')
run('save', '-qam', 'Rename', cwd=work)
retire()
assert 'file.txt' in run('log', '-1', '--name-status', '--', 'file.txt', cwd=work)
print('initial and edited object-only states are visible to file history and patches')
PY
eq "file history reads retired object states" 0 "$?"
finish
