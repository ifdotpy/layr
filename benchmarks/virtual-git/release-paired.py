#!/usr/bin/env python3
"""Compare release binaries on one FSKit mount per case, after an initial Git export."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time

p = argparse.ArgumentParser()
p.add_argument('--before', required=True)
p.add_argument('--after', required=True)
p.add_argument('--out', required=True)
p.add_argument('--rounds', type=int, default=15)
p.add_argument('--files', type=int, nargs='+', default=[1000, 10000])
a = p.parse_args()
assert os.getuid() != 0
work = Path(tempfile.mkdtemp(prefix='layr-git-release-', dir='/private/tmp')).resolve()
env = dict(os.environ, LAYR_ROOT=str(work / 'store'), TMPDIR=str(work),
           LAYR_EXPERIMENTAL_VIEWS='1', LAYR_AUTHOR_NAME='Benchmark',
           LAYR_AUTHOR_EMAIL='benchmark@example.invalid',
           GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1')
for key in ['LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET']:
    env.pop(key, None)

def run(binary, cwd, *args):
    start = time.perf_counter_ns()
    proc = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True)
    ms = (time.perf_counter_ns() - start) / 1e6
    if proc.returncode:
        raise RuntimeError(f'{args}: {proc.stderr.decode()} {proc.stdout.decode()}')
    return ms, proc.stdout.decode()

result = {'platform': platform.platform(), 'work': str(work), 'rounds': a.rounds,
          'warmup_pairs': 2, 'profile': 'release', 'started_at_unix': time.time(), 'cases': [],
          'binaries': {name: {'path': binary, 'sha256': hashlib.sha256(Path(binary).read_bytes()).hexdigest()}
                       for name, binary in [('before', a.before), ('after', a.after)]}}

def persist():
    Path(a.out).write_text(json.dumps(result, indent=2) + '\n')

for count in a.files:
    source = work / f'source-{count}'
    source.mkdir()
    for i in range(count):
        folder = source / f'd{i // 100:04d}'
        folder.mkdir(exist_ok=True)
        (folder / f'f{i:05d}.txt').write_text(f'file {i}\n' + 'unchanged source line\n' * 48)
    project = f'release-{count}'
    run(a.after, work, 'init', project, '--from', str(source), '-q')
    base = work / 'store/projects' / project / 'lines/main'
    run(a.after, base, 'migrate', '--delete-old')
    run(a.after, base, 'line', 'new', 'paired', '--view', 'virtual')
    line = base.parent / 'paired'
    case = {'files': count, 'changed_files': 5, 'samples': []}
    result['cases'].append(case)
    try:
        case['initial_sync_ms'], _ = run(a.after, line, 'git', 'sync', '-q')
        for trial in range(-2, a.rounds):
            for side, name in enumerate(['before', 'after'] if trial % 2 == 0 else ['after', 'before']):
                binary = a.before if name == 'before' else a.after
                for i in range(5):
                    (line / f'd0000/f{i:05d}.txt').write_text(f'paired {trial} {side} {i}\n' + 'source line\n' * 48)
                row = {'trial': trial, 'binary': name, 'load': list(os.getloadavg()), 'started_at_unix': time.time()}
                row['status_ms'], status = run(binary, line, 'status', '--porcelain')
                assert len(status.splitlines()) == 5, status
                row['diff_ms'], diff = run(binary, line, 'diff')
                assert 'paired' in diff
                row['save_ms'], _ = run(binary, line, 'save', '-q', '-m', f'paired {trial} {side}')
                row['finished_at_unix'] = time.time()
                _, clean = run(binary, line, 'status', '--porcelain')
                assert not clean.strip(), clean
                assert not (line / '.git').exists()
                if trial >= 0:
                    case['samples'].append(row)
                    persist()
        case['medians'] = {name: {op: statistics.median(s[f'{op}_ms'] for s in case['samples'] if s['binary'] == name)
                                 for op in ['status', 'diff', 'save']} for name in ['before', 'after']}
        persist()
        print(json.dumps({'files': count, 'medians': case['medians']}), flush=True)
    finally:
        run(a.after, base, 'line', 'show', 'paired', '--none')
