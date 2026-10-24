#!/usr/bin/env python3
"""Run the same CLI workload on an existing, empty backend test directory."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--root', required=True)
p.add_argument('--files', type=int, default=1000)
p.add_argument('--repeats', type=int, default=3)
p.add_argument('--trace', action='store_true')
a = p.parse_args()
root = Path(a.root).resolve()
root.mkdir(parents=True, exist_ok=False)
env = dict(os.environ, LAYR_ROOT=str(root / 'store'), LAYR_AUTHOR_NAME='Benchmark', LAYR_AUTHOR_EMAIL='bench@example.invalid')
for key in ('LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET'):
    env.pop(key, None)
binary = str(Path(a.binary).resolve())
binary_sha256 = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
results = []
commands = []
if a.trace: env["LAYR_TRACE"] = "1"

def run(cwd, *args):
    start = time.perf_counter()
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    r = subprocess.run([binary, *args], cwd=cwd, env=env, text=True, capture_output=True)
    seconds = time.perf_counter() - start
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    commands.append({"args": args, "wall": seconds, "cpu": after.ru_utime+after.ru_stime-before.ru_utime-before.ru_stime, "stderr": r.stderr, "load": os.getloadavg()})
    if r.returncode:
        raise RuntimeError(f'{args}: {r.returncode}: {r.stderr}')
    return seconds, r.stdout

src = root / 'source'
src.mkdir()
digest = hashlib.sha256()
for n in range(a.files):
    path = src / f'd{n // 100:05}' / f'f{n:07}.txt'
    path.parent.mkdir(exist_ok=True)
    data = (f'file {n:07} stable line\n'.encode() * 200)[:4096]
    path.write_bytes(data)
    digest.update(data)
for trial in range(a.repeats):
    project = f'p{trial}'
    row = {'trial': trial}
    row['init'], _ = run(root, 'init', project, '--from', str(src), '-q')
    main = root / 'store' / 'projects' / project / 'lines' / 'main'
    run(main, 'protect', 'main', '--remove')
    row['branch'], _ = run(main, 'line', 'new', '-q', 'topic')
    topic = main.parent / 'topic'
    row['status_clean'], status = run(topic, 'status', '--porcelain')
    assert not status.strip(), status
    for n in range(min(10, a.files)):
        path = topic / f'd{n // 100:05}' / f'f{n:07}.txt'
        with path.open('ab') as f:
            f.write(b'changed\n')
    row['diff_changed'], _ = run(topic, 'diff', '--stat')
    row['add'], _ = run(topic, 'add', '-A')
    row['commit'], _ = run(topic, 'save', '-qm', 'ten files')
    row['merge'], _ = run(main, 'merge', '-q', 'topic')
    _, status = run(main, 'status', '--porcelain')
    assert not status.strip(), status
    row['fsck'], _ = run(main, 'fsck')
    results.append(row)
    report = {'platform': platform.platform(), 'files': a.files, 'binary_sha256': binary_sha256, 'fixture_sha256': digest.hexdigest(), 'results': results, 'commands': commands}
    (root / 'results.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(row), flush=True)
