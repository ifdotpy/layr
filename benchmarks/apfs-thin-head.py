#!/usr/bin/env python3
"""Time `line new` from a thin head that has no complete view, and a fast-forward `merge` of one changed file.

Each binary gets its own store. The trials alternate the order of the binaries, so both see the same host load.

    python3 -I benchmarks/apfs-thin-head.py --root <new dir> --bin base=<layr> --bin new=<layr> --repeats 6
"""
import argparse, json, os, subprocess, time, statistics
from pathlib import Path
p = argparse.ArgumentParser()
p.add_argument('--root', required=True)
p.add_argument('--bin', action='append', required=True)
p.add_argument('--files', type=int, default=10000)
p.add_argument('--repeats', type=int, default=3)
a = p.parse_args()
root = Path(a.root); root.mkdir(parents=True, exist_ok=False)
src = root / 'source'; src.mkdir()
for n in range(a.files):
    path = src / f'd{n // 100:05}' / f'f{n:07}.txt'
    path.parent.mkdir(exist_ok=True)
    path.write_bytes((f'file {n:07} stable line\n'.encode() * 200)[:4096])
bins = [b.split('=', 1) for b in a.bin]
def env(name):
    e = dict(os.environ, LAYR_ROOT=str(root / f'store-{name}'), LAYR_AUTHOR_NAME='B', LAYR_AUTHOR_EMAIL='b@example.invalid')
    for k in ('LAYR_PROJECT', 'LAYR_LINE', 'LAYR_SOCKET', 'LAYR_TRACE'): e.pop(k, None)
    return e
def run(name, binary, cwd, *args):
    t = time.perf_counter()
    r = subprocess.run([binary, *args], cwd=cwd, env=env(name), text=True, capture_output=True)
    w = time.perf_counter() - t
    if r.returncode: raise RuntimeError(f'{name} {args}: {r.returncode}: {r.stderr[-3000:]}')
    return w
for name, b in bins:
    run(name, b, root, 'init', 'p', '--from', str(src), '-q')
    main = root / f'store-{name}' / 'projects' / 'p' / 'lines' / 'main'
    run(name, b, main, 'protect', 'main', '--remove')
    run(name, b, main, 'line', 'new', '-q', 'topic')
res = {n: {'line_new_thin': [], 'merge_ff': []} for n, _ in bins}
for t in range(a.repeats):
    for name, b in (bins if t % 2 == 0 else bins[::-1]):
        lines = root / f'store-{name}' / 'projects' / 'p' / 'lines'
        topic = lines / 'topic'
        with (topic / 'd00000' / f'f{t:07}.txt').open('ab') as fh: fh.write(f'change {t}\n'.encode())
        run(name, b, topic, 'save', '-qam', f'change {t}')
        res[name]['line_new_thin'].append(run(name, b, topic, 'line', 'new', '-q', f'n{t}'))
        res[name]['merge_ff'].append(run(name, b, lines / 'main', 'merge', '-q', 'topic'))
        assert (lines / 'main' / 'd00000' / f'f{t:07}.txt').read_bytes() == (topic / 'd00000' / f'f{t:07}.txt').read_bytes()
        assert (lines / f'n{t}' / 'd00000' / f'f{t:07}.txt').read_bytes() == (topic / 'd00000' / f'f{t:07}.txt').read_bytes()
        print(name, t, {k: round(v[-1], 2) for k, v in res[name].items()}, 'load %.0f' % os.getloadavg()[0], flush=True)
for name, r in res.items():
    print('MEDIAN', name, {k: round(statistics.median(v), 3) for k, v in r.items()})
(root / 'results.json').write_text(json.dumps(res, indent=1))
