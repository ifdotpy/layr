#!/usr/bin/env python3
"""Verify thin APFS states: commits, automatic states, lines, views, gc, and merges."""
import argparse, os, shutil, sqlite3, stat, subprocess, tempfile
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--root', required=True)
a = p.parse_args()
binary = str(Path(a.binary).resolve())
root = Path(tempfile.mkdtemp(prefix='apfs-thin-', dir=Path(a.root).resolve()))
env = dict(os.environ, LAYR_ROOT=str(root / 'store'), LAYR_AUTHOR_NAME='Test', LAYR_AUTHOR_EMAIL='test@example.invalid')
for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE', 'LAYR_TRACE'):
    env.pop(key, None)


def run(cwd, *args, ok=True):
    r = subprocess.run([binary, *map(str, args)], cwd=cwd, env=env, capture_output=True, text=True)
    assert (r.returncode == 0) == ok, (args, r.returncode, r.stdout, r.stderr)
    return r


def tree(path):
    """Type, mode, and content of every entry except .git."""
    out = {}
    for dirpath, dirs, files in os.walk(path):
        rel_dir = os.path.relpath(dirpath, path)
        if rel_dir == '.git' or rel_dir.startswith('.git/'):
            continue
        dirs[:] = [d for d in dirs if not (rel_dir == '.' and d == '.git')]
        for name in dirs + files:
            full = os.path.join(dirpath, name)
            rel = os.path.normpath(os.path.join(rel_dir, name))
            m = os.lstat(full)
            if stat.S_ISLNK(m.st_mode):
                out[rel] = ('link', os.readlink(full))
            elif stat.S_ISDIR(m.st_mode):
                if not os.path.ismount(full) and not os.listdir(full):
                    out[rel] = ('dir',)
            else:
                out[rel] = ('file', m.st_mode & 0o111 != 0, Path(full).read_bytes())
    return out


def head(work):
    """The state id of the head of the line of `work`."""
    with sqlite3.connect(root / 'store/projects/p/meta/db/layr.sqlite') as c:
        return c.execute('SELECT head FROM lines WHERE name=?', (work.name,)).fetchone()[0]


try:
    src = root / 'source'
    for i in range(2000):
        f = src / f'd{i // 100}' / f'f{i}'
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_bytes(f'file {i}\n'.encode())
    (src / 'tool.sh').write_bytes(b'#!/bin/sh\n')
    os.symlink('d0/f0', src / 'link')
    run(root, 'init', 'p', '--from', src, '-q')
    project = root / 'store/projects/p'
    main = project / 'lines/main'
    run(main, 'protect', 'main', '--remove')
    run(main, 'line', 'new', '-q', 'topic')
    work = main.parent / 'topic'

    # A commit stores only the changed entries.
    (work / 'd0/f1').write_bytes(b'changed\n')
    (work / 'd1/f100').unlink()
    os.rename(work / 'd2/f200', work / 'd2/moved')
    (work / 'new/deep').mkdir(parents=True)
    (work / 'new/deep/file').write_bytes(b'new\n')
    os.chmod(work / 'tool.sh', 0o755)
    (work / 'link').unlink()
    os.symlink('d0/f1', work / 'link')
    shutil.rmtree(work / 'd3')
    (work / 'd3').write_bytes(b'a file where a folder was\n')
    run(work, 'add', '-A')
    status = run(work, 'status', '--porcelain').stdout
    assert 'R  d2/f200 -> d2/moved' in status and 'D  d1/f100' in status, status
    run(work, 'save', '-qm', 'thin commit')
    h1 = head(work)
    layer = project / 'deltas' / h1
    assert layer.is_dir() and not (project / 'states' / h1).exists(), (h1, sorted(os.listdir(project / 'deltas')), sorted(os.listdir(project / 'states')))
    stored = sum(len(files) for _, _, files in os.walk(layer))
    assert stored <= 6, f'the layer holds {stored} files'
    left = run(work, 'status', '--porcelain').stdout
    assert not left, left
    assert run(work, 'show', 'head:d0/f1').stdout == 'changed\n'
    diff = run(work, 'diff', '--stat', 'head~1', 'head').stdout
    assert 'd1/f100' in diff and 'new/deep/file' in diff, diff
    views = set(os.listdir(project / 'states'))
    log = run(work, 'log', '-p', '--', 'd0/f1').stdout
    assert '+changed' in log and set(os.listdir(project / 'states')) == views, 'log made a view'
    print('PASS a commit stores only its changes', flush=True)

    # mv after a thin add makes the index complete and keeps the staged change.
    (work / 'd6/f601').write_bytes(b'staged first\n')
    run(work, 'add', 'd6/f601')
    run(work, 'mv', 'd6/f600', 'd6/renamed')
    status = run(work, 'status', '--porcelain').stdout
    assert 'M  d6/f601' in status and 'R  d6/f600 -> d6/renamed' in status, status
    run(work, 'save', '-qm', 'complete index')
    assert run(work, 'show', 'head:d6/f601').stdout == 'staged first\n'
    assert run(work, 'show', 'head:d6/renamed').stdout == 'file 600\n'
    assert not run(work, 'status', '--porcelain').stdout
    print('PASS mv after a thin add keeps the staged change', flush=True)

    # A staged removal in a folder keeps the folder and its other files.
    (work / 'd7/f700').unlink()
    run(work, 'add', '-A')
    run(work, 'save', '-qm', 'remove one file of a folder')
    assert run(work, 'show', 'head:d7/f701').stdout == 'file 701\n'

    # A line made from a thin head has the complete content.
    run(work, 'line', 'new', '-q', 'second')
    assert tree(main.parent / 'second') == tree(work)
    print('PASS a line made from a thin head has the complete content', flush=True)

    # Automatic states are thin too; an unchanged folder makes none.
    (work / 'd4').joinpath('f400').write_bytes(b'not committed\n')
    known = set(os.listdir(project / 'deltas'))
    run(work, 'save', '-q')
    autos = set(os.listdir(project / 'deltas')) - known
    assert len(autos) == 1, autos
    run(work, 'save', '-q')
    assert set(os.listdir(project / 'deltas')) - known == autos
    (work / 'd4/f400').write_bytes(b'file 400\n')
    print('PASS automatic states are thin, and an unchanged folder makes none', flush=True)

    # gc removes unused views and gives a long chain a complete tree.
    run(work, 'config', 'thin.view_seconds', '0')
    run(work, 'config', 'thin.chain', '3')
    run(work, 'show', 'head')
    run(work, 'diff', 'head~1', 'head', '--', 'd0/f1')
    for n in range(4):
        (work / 'd5' / f'f{500 + n}').write_bytes(f'commit {n}\n'.encode())
        run(work, 'save', '-qam', f'step {n}')
    before = tree(work)
    run(work, 'gc')
    h = head(work)
    assert (project / 'states' / h).is_dir() and not (project / 'deltas' / h).exists()
    assert not (project / 'states' / h1).exists(), 'gc keeps an unused view'
    assert tree(work) == before and not run(work, 'status', '--porcelain').stdout
    assert run(work, 'show', 'head~3:d5/f500').stdout == 'commit 0\n'
    print('PASS gc removes unused views and shortens long chains', flush=True)

    # A changed file with two names makes a complete automatic state, which keeps both names.
    os.link(work / 'd9/f900', work / 'hard')
    with open(work / 'hard', 'ab') as f:
        f.write(b'both names\n')
    known = set(os.listdir(project / 'states'))
    run(work, 'save', '-q')
    made = [d for d in set(os.listdir(project / 'states')) - known if '@' not in d]
    assert len(made) == 1, made
    auto = project / 'states' / made[0]
    assert os.lstat(auto / 'hard').st_ino == os.lstat(auto / 'd9/f900').st_ino
    os.unlink(work / 'hard')
    run(work, 'save', '-qam', 'after a linked change')
    print('PASS a changed file with two names makes a complete state', flush=True)

    # Commands that copy the working folder succeed while a process writes in it.
    import threading
    stop = threading.Event()
    def write():
        n = 0
        while not stop.is_set():
            (work / f'd9/f{900 + n % 50}').write_bytes(f'busy {n}\n'.encode())
            (work / f'busy{n % 5}').write_bytes(b'x')
            try:
                (work / f'busy{(n + 2) % 5}').unlink()
            except FileNotFoundError:
                pass
            n += 1
    t = threading.Thread(target=write)
    t.start()
    try:
        run(work, 'save', '-q')
        run(work, 'add', '-A')
        run(work, 'save', '-qm', 'during writes')
        run(work, 'line', 'new', '-q', 'busy-copy')
    finally:
        stop.set()
        t.join()
    for n in range(5):
        (work / f'busy{n}').unlink(missing_ok=True)
    run(work, 'add', '-A')
    run(work, 'save', '-qm', 'after writes')
    print('PASS copies succeed while files change', flush=True)

    # A merge reads thin states and its result matches the line.
    run(main, 'merge', '-q', 'topic')
    assert tree(main) == tree(work)
    run(main, 'fsck')
    run(work, 'fsck')
    print('PASS a merge of thin states matches the line, and fsck passes', flush=True)
finally:
    subprocess.run(['chflags', '-R', 'nouchg', str(root)], check=True)
    shutil.rmtree(root)
