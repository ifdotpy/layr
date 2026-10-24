#!/usr/bin/env python3
"""Compare two binaries on the same btrfs project, with alternating command order.

Run inside Linux as root. The root must not exist. Results include all samples.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import statistics
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--before', required=True)
p.add_argument('--after', required=True)
p.add_argument('--root', required=True)
p.add_argument('--files', type=int, default=10000)
p.add_argument('--repeats', type=int, default=12)
a = p.parse_args()
root = Path(a.root).resolve()
root.mkdir()
source = root / 'source'
source.mkdir()
fixture = hashlib.sha256()
for i in range(a.files):
    path = source / f'd{i // 100}' / f'f{i:06d}.txt'
    path.parent.mkdir(exist_ok=True)
    content = (f'{i:08d} source content\n' * 256).encode()[:4096]
    path.write_bytes(content)
    fixture.update(str(path.relative_to(source)).encode() + b'\0' + content)
env = dict(os.environ, LAYR_ROOT=str(root / 'store'), LAYR_AUTHOR_NAME='Benchmark', LAYR_AUTHOR_EMAIL='bench@example.com')
for key in ['LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET']:
    env.pop(key, None)
binaries = {'before': str(Path(a.before).resolve()), 'after': str(Path(a.after).resolve())}

def run(which, args, cwd=source):
    cpu = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.perf_counter()
    result = subprocess.run([binaries[which], *args], cwd=cwd, env=env, capture_output=True, check=True)
    elapsed = time.perf_counter() - start
    used = resource.getrusage(resource.RUSAGE_CHILDREN)
    return elapsed, result.stdout, used.ru_utime + used.ru_stime - cpu.ru_utime - cpu.ru_stime

run('before', ['init', 'bench', '--from', str(source), '-q'])
work = root / 'store/projects/bench/lines/main'
base = run('before', ['rev-parse', 'head'], work)[1].decode().strip()
for i in range(10):
    with (work / f'd{i // 100}' / f'f{i:06d}.txt').open('a') as f:
        f.write('change\n')
run('before', ['save', '-qam', 'change'], work)
head = run('before', ['rev-parse', 'head'], work)[1].decode().strip()
commands = {'status_clean': ['status', '--porcelain'], 'diff_states': ['diff', base, head], 'save_clean': ['save', '-q']}
results = {'files': a.files, 'fixture_sha256': fixture.hexdigest(), 'binaries': {}, 'samples': []}
for which, binary in binaries.items():
    results['binaries'][which] = {'path': binary, 'sha256': hashlib.sha256(Path(binary).read_bytes()).hexdigest()}
# Separate the first cache population from the subsequent samples.
results['diff_first_seconds'], first, results['diff_first_cpu_seconds'] = run('after', commands['diff_states'], work)
assert first == run('before', commands['diff_states'], work)[1]
for trial in range(a.repeats):
    for name, command in commands.items():
        outputs = []
        for which in (['before', 'after'] if trial % 2 == 0 else ['after', 'before']):
            seconds, output, cpu = run(which, command, work)
            outputs.append(output)
            row = {'trial': trial, 'command': name, 'binary': which, 'seconds': seconds, 'cpu_seconds': cpu, 'load': os.getloadavg()}
            results['samples'].append(row)
        assert outputs[0] == outputs[1], f'{name}: outputs differ'
    (root / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
results['medians'] = {
    name: {which: statistics.median(row['seconds'] for row in results['samples'] if row['command'] == name and row['binary'] == which)
           for which in binaries} for name in commands
}
results['cpu_medians'] = {
    name: {which: statistics.median(row['cpu_seconds'] for row in results['samples'] if row['command'] == name and row['binary'] == which)
           for which in binaries} for name in commands
}
(root / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
print(json.dumps({'wall': results['medians'], 'cpu': results['cpu_medians']}))
