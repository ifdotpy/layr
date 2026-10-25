#!/usr/bin/env python3
"""Compare everyday commands of git and one or more layr binaries on the same files.

Each run makes a new project of `--files` small files below `--root`, then times: import of the
folder, a clean status (first and second run), a status and a diff with 100 changed files, add
and commit of them, a diff of two states, and a new branch with its own folder. Tools run in
turns, in reverse order every other run. The medians in milliseconds go to stdout as JSON.

Use an idle machine: the load of other programs changes the results more than the code does.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time

p = argparse.ArgumentParser()
p.add_argument('--root', required=True, help='a folder for the test data (any APFS folder)')
p.add_argument('--layr', action='append', required=True, metavar='NAME=BINARY', help='a layr binary; repeat to compare')
p.add_argument('--files', type=int, default=10000)
p.add_argument('--repeats', type=int, default=5)
a = p.parse_args()
tools = dict(x.split('=', 1) for x in a.layr)
base = Path(tempfile.mkdtemp(prefix='git-compare-', dir=Path(a.root).resolve()))
src = base / 'src'
for i in range(a.files):
    d = src / f'd{i % 100}' / f'e{i % 10}'
    d.mkdir(parents=True, exist_ok=True)
    (d / f'f{i}.txt').write_text(f'line {i}\n' * 40)
env = dict(os.environ, LAYR_AUTHOR_NAME='T', LAYR_AUTHOR_EMAIL='t@example.invalid', GIT_AUTHOR_NAME='T',
           GIT_AUTHOR_EMAIL='t@example.invalid', GIT_COMMITTER_NAME='T', GIT_COMMITTER_EMAIL='t@example.invalid')
for k in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE', 'LAYR_TRACE'):
    env.pop(k, None)
res = {}


def t(tool, op, cwd, *cmd):
    s = time.perf_counter()
    r = subprocess.run(list(cmd), cwd=cwd, env=env, capture_output=True, text=True)
    assert r.returncode == 0, (tool, cmd, r.stderr[-400:])
    res.setdefault(op, {}).setdefault(tool, []).append(time.perf_counter() - s)


def edit(root):
    for i in range(0, a.files, max(1, a.files // 100)):
        f = root / f'd{i % 100}' / f'e{i % 10}' / f'f{i}.txt'
        f.write_text(f.read_text() + 'x\n')


def run_git(n):
    g = base / f'git{n}'
    shutil.copytree(src, g)
    t('git', 'import folder', g, 'sh', '-c', 'git init -q && git add -A && git commit -qm init')
    t('git', 'status clean', g, 'git', 'status', '--porcelain')
    t('git', 'status clean 2nd', g, 'git', 'status', '--porcelain')
    edit(g)
    t('git', 'status 100 changed', g, 'git', 'status', '--porcelain')
    t('git', 'diff --stat working', g, 'git', 'diff', '--stat', 'HEAD')
    t('git', 'add + commit 100', g, 'sh', '-c', 'git add -A && git commit -qm c')
    t('git', 'diff --stat states', g, 'git', 'diff', '--stat', 'HEAD~1', 'HEAD')
    t('git', 'new branch + folder', g, 'git', 'worktree', 'add', '-q', str(base / f'git{n}-b'), '-b', 'b')


def run_layr(tool, b, n):
    env['LAYR_ROOT'] = str(base / f'{tool}{n}')
    t(tool, 'import folder', base, b, 'init', 'p', '--from', str(src), '-q')
    m = base / f'{tool}{n}/projects/p/lines/main'
    subprocess.run([b, 'protect', 'main', '--remove'], cwd=m, env=env, capture_output=True, check=True)
    t(tool, 'status clean', m, b, 'status', '--porcelain')
    t(tool, 'status clean 2nd', m, b, 'status', '--porcelain')
    edit(m)
    t(tool, 'status 100 changed', m, b, 'status', '--porcelain')
    t(tool, 'diff --stat working', m, b, 'diff', '--stat', 'head')
    t(tool, 'add + commit 100', m, 'sh', '-c', f'"{b}" add -A && "{b}" save -qm c')
    t(tool, 'diff --stat states', m, b, 'diff', '--stat', 'head~1', 'head')
    t(tool, 'new branch + folder', m, b, 'line', 'new', '-q', 'b')


names = ['git', *tools]
for n in range(a.repeats):
    for tool in names if n % 2 == 0 else reversed(names):
        run_git(n) if tool == 'git' else run_layr(tool, tools[tool], n)
print(json.dumps({op: {k: round(statistics.median(v) * 1000) for k, v in d.items()} for op, d in res.items()}, indent=1))
print(f'test data: {base} (states are immutable: chflags -R nouchg before rm -rf)')
