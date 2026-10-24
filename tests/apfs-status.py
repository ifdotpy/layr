#!/usr/bin/env python3
"""Compare APFS status with a baseline binary, including live mmap.

Each binary uses its own store: the store formats of two versions can differ.
Every change goes to both working folders.
"""
import argparse
import mmap
import os
from pathlib import Path
import subprocess
import tempfile

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--baseline', required=True)
p.add_argument('--root', required=True)
a = p.parse_args()
binary, baseline = str(Path(a.binary).resolve()), str(Path(a.baseline).resolve())
root = Path(tempfile.mkdtemp(prefix='apfs-status-', dir=a.root))
envs = []
for i in range(2):
    env = dict(os.environ, LAYR_ROOT=str(root / f'store{i}'), LAYR_TRACE='1',
               LAYR_AUTHOR_NAME='Test', LAYR_AUTHOR_EMAIL='test@example.invalid')
    for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE'):
        env.pop(key, None)
    envs.append(env)
executables = [baseline, binary]


def run(i, cwd, *args):
    r = subprocess.run([executables[i], *args], cwd=cwd, env=envs[i], capture_output=True, text=True)
    assert r.returncode == 0, (i, args, r.returncode, r.stdout, r.stderr)
    return r


def both(*args):
    for i in range(2):
        run(i, works[i], *args)


checks = 0

def same_status(label, *, fallback=False):
    global checks
    before = run(0, works[0], 'status', '--porcelain', '--ignored')
    after = run(1, works[1], 'status', '--porcelain', '--ignored')
    assert before.stdout == after.stdout, (label, before.stdout, after.stdout)
    # A status can come from the saved status of the previous one instead.
    if 'saved status used' not in after.stderr:
        assert 'APFS validated read' in after.stderr, (label, after.stderr)
        assert ('APFS clone ' in after.stderr) == fallback, (label, after.stderr)
    checks += 1
    print('PASS', label, flush=True)
    return after.stdout


try:
    source = root / 'source'
    source.mkdir()
    (source / 'file').write_bytes(b'a' * 16384)
    (source / 'readonly').write_text('old\n')
    (source / 'readonly').chmod(0o444)
    (source / 'atomic').write_text('old\n')
    (source / 'mode').write_text('mode\n')
    (source / 'remove').write_text('remove\n')
    (source / 'link').symlink_to('file')
    (source / '.gitignore').write_text('ignored/\n')
    works, mains = [], []
    for i in range(2):
        run(i, root, 'init', 'test', '--from', str(source), '-q')
        mains.append(root / f'store{i}/projects/test/lines/main')
        run(i, mains[i], 'line', 'new', '-q', 'topic')
        works.append(mains[i].parent / 'topic')
    assert not same_status('clean status')

    for work in works:
        old = (work / 'file').stat()
        with (work / 'file').open('r+b') as f:
            f.seek(8192)
            f.write(b'x')
        os.utime(work / 'file', ns=(old.st_atime_ns, old.st_mtime_ns))
    assert ' M file' in same_status('equal size and restored mtime')
    both('add', 'file')
    assert 'M  file' in same_status('staged file')
    for work in works:
        with (work / 'file').open('r+b') as f:
            f.write(b'y')
    assert 'MM file' in same_status('staged and unstaged bytes')

    for work in works:
        old = (work / 'atomic').stat()
        (work / 'replacement').write_text('new\n')
        os.utime(work / 'replacement', ns=(old.st_atime_ns, old.st_mtime_ns))
        os.replace(work / 'replacement', work / 'atomic')
    assert ' M atomic' in same_status('atomic replacement with restored mtime')
    for work in works:
        (work / 'mode').chmod(0o755)
        (work / 'remove').unlink()
        (work / 'link').unlink()
        (work / 'link').symlink_to('atomic')
        (work / 'ignored').mkdir()
        (work / 'ignored/data').write_text('ignored\n')
        (work / 'new-dir').mkdir()
        (work / 'new-dir/новый').write_text('new\n')
    same_status('mode, deletion, links, ignored and Unicode paths')

    for work in works:
        (work / 'readonly').chmod(0o644)
        (work / 'readonly').write_text('new\n')
        (work / 'readonly').chmod(0o444)
    both('add', '-A')
    same_status('index replacement of read-only files')
    both('save', '-qm', 'changes')
    for i in range(2):
        assert not run(i, works[i], 'status', '--porcelain').stdout.strip()
        assert (works[i] / 'readonly').stat().st_mode & 0o777 == 0o444
        assert (mains[i] / 'readonly').read_text() == 'old\n'
        run(i, works[i], 'fsck')

    with (works[0] / 'file').open('r+b') as f0, (works[1] / 'file').open('r+b') as f1:
        with mmap.mmap(f0.fileno(), 0) as m0, mmap.mmap(f1.fileno(), 0) as m1:
            for mapped in (m0, m1):
                mapped[4096:4097] = b'z'
                mapped.flush()
            assert ' M file' in same_status('live mmap uses snapshot fallback', fallback=True)
    same_status('unmapped file returns to validated read')
    assert not list((root / 'store1').rglob('.layr-copy-*'))
    print(f'{checks} status comparisons passed', flush=True)
finally:
    subprocess.run(['chflags', '-R', 'nouchg', str(root)], check=True)
    import shutil
    shutil.rmtree(root)
