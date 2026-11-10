#!/usr/bin/env python3
"""Paired CLI timings on real FSKit mounts. Run as a native user."""
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
p.add_argument('--rounds', type=int, default=9)
p.add_argument('--files', type=int, nargs='+', default=[1000])
a = p.parse_args()
assert os.getuid() != 0
work = Path(tempfile.mkdtemp(prefix='layr-virtual-git-', dir='/private/tmp')).resolve()
env = dict(os.environ, LAYR_ROOT=str(work / 'store'), TMPDIR=str(work),
           LAYR_EXPERIMENTAL_VIEWS='1', LAYR_AUTHOR_NAME='Benchmark',
           LAYR_AUTHOR_EMAIL='benchmark@example.invalid', GIT_CONFIG_GLOBAL='/dev/null',
           GIT_CONFIG_NOSYSTEM='1')
for key in ['LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET']:
    env.pop(key, None)

def run(binary, cwd, *args):
    start = time.perf_counter_ns()
    result = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True)
    elapsed = (time.perf_counter_ns() - start) / 1e6
    if result.returncode:
        raise RuntimeError(f'{args}: {result.stderr.decode()} {result.stdout.decode()}')
    return elapsed, result.stdout.decode()

result = {'platform': platform.platform(), 'work': str(work), 'rounds': a.rounds,
          'binaries': {k: {'path': b, 'sha256': hashlib.sha256(Path(b).read_bytes()).hexdigest()}
                       for k, b in [('before', a.before), ('after', a.after)]}, 'cases': []}
for count in a.files:
    source = work / f'source-{count}'
    source.mkdir()
    for i in range(count):
        folder = source / f'd{i // 100:04d}'
        folder.mkdir(exist_ok=True)
        (folder / f'f{i:05d}.txt').write_text(f'file {i}\n' + 'unchanged source line\n' * 48)
    project = f'bench-{count}'
    run(a.after, work, 'init', project, '--from', str(source), '-q')
    base = work / 'store/projects' / project / 'lines/main'
    run(a.after, base, 'migrate', '--delete-old')
    paths = {}
    for name in ['before', 'after']:
        run(a.after, base, 'line', 'new', name, '--view', 'virtual')
        paths[name] = base.parent / name
    case = {'files': count, 'changed_files': 5, 'samples': []}
    # Warm process and filesystem caches without a Git export.
    for name, binary in [('before', a.before), ('after', a.after)]:
        run(binary, paths[name], 'status', '--porcelain')
        run(binary, paths[name], 'diff')
    for phase in ['without_git_export', 'with_git_export']:
        if phase == 'with_git_export':
            case['first_sync_ms'], _ = run(a.after, paths['after'], 'git', 'sync', '-q')
        for trial in range(a.rounds):
            names = ['before', 'after'] if trial % 2 == 0 else ['after', 'before']
            for name in names:
                binary = a.before if name == 'before' else a.after
                cwd = paths[name]
                for i in range(5):
                    (cwd / f'd0000/f{i:05d}.txt').write_text(f'{phase} {trial} file {i}\n' + 'source line\n' * 48)
                sample = {'phase': phase, 'trial': trial, 'binary': name}
                sample['status_ms'], status = run(binary, cwd, 'status', '--porcelain')
                assert len(status.splitlines()) == 5, status
                sample['diff_ms'], diff = run(binary, cwd, 'diff')
                assert phase in diff
                sample['save_ms'], _ = run(binary, cwd, 'save', '-q', '-m', f'{phase} {trial}')
                _, clean = run(binary, cwd, 'status', '--porcelain')
                assert not clean.strip(), clean
                assert not (cwd / '.git').exists()
                if name == 'after' and phase == 'with_git_export':
                    sample['sync_ms'], _ = run(binary, cwd, 'git', 'sync', '-q')
                case['samples'].append(sample)
    case['medians'] = {phase: {name: {op: statistics.median(
        s[f'{op}_ms'] for s in case['samples'] if s['phase'] == phase and s['binary'] == name)
        for op in ['status', 'diff', 'save']} for name in ['before', 'after']}
        for phase in ['without_git_export', 'with_git_export']}
    case['incremental_sync_median_ms'] = statistics.median(s['sync_ms'] for s in case['samples'] if 'sync_ms' in s)
    result['cases'].append(case)
    Path(a.out).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k: v for k, v in case.items() if k != 'samples'}), flush=True)
    for name in paths:
        run(a.after, base, 'line', 'show', name, '--none')
print(f'Evidence: {a.out}; private store retained at {work}', flush=True)
