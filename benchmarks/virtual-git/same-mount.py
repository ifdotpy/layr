#!/usr/bin/env python3
"""Alternate both binaries on one existing virtual line after a Git export."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--before', required=True)
p.add_argument('--after', required=True)
p.add_argument('--line', required=True)
p.add_argument('--out', required=True)
p.add_argument('--rounds', type=int, default=9)
a = p.parse_args()
line = Path(a.line).resolve()
root = line.parents[3]
env = dict(os.environ, LAYR_ROOT=str(root), TMPDIR=str(root.parent),
           LAYR_EXPERIMENTAL_VIEWS='1', LAYR_AUTHOR_NAME='Benchmark',
           LAYR_AUTHOR_EMAIL='benchmark@example.invalid', GIT_CONFIG_GLOBAL='/dev/null')
for key in ['LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET']:
    env.pop(key, None)
assert os.getuid() != 0

def run(binary, cwd, *args):
    start = time.perf_counter_ns()
    proc = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, check=True)
    return (time.perf_counter_ns() - start) / 1e6, proc.stdout.decode()

run(a.after, line.parent / 'main', 'line', 'show', line.name, '--virtual')
result = {'line': str(line), 'rounds': a.rounds, 'samples': [],
          'binaries': {name: hashlib.sha256(Path(binary).read_bytes()).hexdigest()
                       for name, binary in [('before', a.before), ('after', a.after)]}}
for trial in range(a.rounds):
    names = ['before', 'after'] if trial % 2 == 0 else ['after', 'before']
    for side, name in enumerate(names):
        binary = a.before if name == 'before' else a.after
        for i in range(5):
            (line / f'd0000/f{i:05d}.txt').write_text(f'same mount {trial} {side} {i}\n' + 'source line\n' * 48)
        row = {'trial': trial, 'binary': name}
        row['status_ms'], status = run(binary, line, 'status', '--porcelain')
        assert len(status.splitlines()) == 5
        row['diff_ms'], diff = run(binary, line, 'diff')
        assert 'same mount' in diff
        row['save_ms'], _ = run(binary, line, 'save', '-q', '-m', f'same mount {trial} {side}')
        assert not (line / '.git').exists()
        result['samples'].append(row)
        Path(a.out).write_text(json.dumps(result, indent=2) + '\n')
result['medians'] = {name: {op: statistics.median(s[f'{op}_ms'] for s in result['samples'] if s['binary'] == name)
                            for op in ['status', 'diff', 'save']} for name in ['before', 'after']}
Path(a.out).write_text(json.dumps(result, indent=2) + '\n')
run(a.after, line.parent / 'main', 'line', 'show', line.name, '--none')
print(json.dumps(result['medians']), flush=True)
