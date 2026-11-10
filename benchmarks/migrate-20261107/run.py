#!/usr/bin/env python3
"""Measure migration in a private temporary store. Never touches the user's Layr store."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--files', type=int, default=10000)
p.add_argument('--states', type=int, default=60)
a = p.parse_args()
if a.files < 1 or a.states < 1:
    raise SystemExit('--files and --states must be positive')
if os.geteuid() == 0:
    raise SystemExit('Do not run this benchmark as root')
binary = str(Path(a.binary).resolve())
root = Path(tempfile.mkdtemp(prefix='layr-migrate-bench-', dir=os.environ['TMPDIR']))
env = dict(os.environ, LAYR_ROOT=str(root / 'store'), LAYR_AUTHOR_NAME='Benchmark',
           LAYR_AUTHOR_EMAIL='benchmark@example.invalid', LAYR_TRASH_BUDGET_MS='2000')
for key in ('LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET'):
    env.pop(key, None)
work = root

def run(*args):
    result = subprocess.run([binary, *args], cwd=work, env=env, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stdout + result.stderr)
    return result.stdout

try:
    source = root / 'source'
    for i in range(a.files):
        file = source / f'd{i // 100}' / f'f{i}.txt'
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(f'file {i}\n' + 'unchanged sample content\n' * 40)
    run('init', 'bench', '--from', str(source), '-q')
    work = root / 'store/projects/bench/lines/main'
    run('protect', 'main', '--remove')
    for i in range(1, a.states):
        file = work / f'd{i % a.files // 100}' / f'f{i % a.files}.txt'
        with file.open('a') as f:
            f.write(f'state {i}\n')
        run('save', '-a', '-m', f'change {i}', '-q')
    print(f'fixture ready: {a.files} files, {a.states} states', flush=True)
    started = time.monotonic()
    print(run('migrate'), flush=True)
    elapsed = time.monotonic() - started
    stat = json.loads(run('objects', 'stat', '--json'))
    print(json.dumps(dict(files=a.files, states=a.states, wall_seconds=elapsed, objects=stat), indent=2), flush=True)
finally:
    subprocess.run(['chflags', '-R', 'nouchg', str(root)], capture_output=True)
    subprocess.run(['/bin/chmod', '-R', '-N', str(root)], capture_output=True)
    subprocess.run(['chmod', '-R', 'u+w', str(root)], capture_output=True)
    shutil.rmtree(root)
