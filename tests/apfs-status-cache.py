#!/usr/bin/env python3
"""Compare status from the saved status with status from a full comparison, after many kinds
of change, including writes through a memory mapping and through a hard link outside the line."""
import argparse, mmap, os, shutil, subprocess, tempfile
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--root', required=True, help='a folder on a case-sensitive APFS volume')
a = p.parse_args()
binary = str(Path(a.binary).resolve())
work_root = Path(tempfile.mkdtemp(prefix='apfs-status-cache-', dir=Path(a.root).resolve()))
env = dict(os.environ, LAYR_ROOT=str(work_root / 'store'), LAYR_AUTHOR_NAME='Test', LAYR_AUTHOR_EMAIL='test@example.invalid')
for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE', 'LAYR_TRACE', 'LAYR_NO_STATUS_CACHE'):
    env.pop(key, None)


def run(cwd, *args, extra=None):
    r = subprocess.run([binary, *map(str, args)], cwd=cwd, env=dict(env, **(extra or {})), capture_output=True, text=True)
    assert r.returncode == 0, (args, r.returncode, r.stdout, r.stderr)
    return r


checks = 0


def same(label, fast=True):
    """The saved status equals the full status; `fast` says the saved status answered."""
    global checks
    quick = run(work, 'status', '--porcelain', '--ignored', extra={'LAYR_TRACE': '1'})
    full = run(work, 'status', '--porcelain', '--ignored', extra={'LAYR_NO_STATUS_CACHE': '1'})
    assert quick.stdout == full.stdout, (label, quick.stdout, full.stdout)
    used = 'saved status used' in quick.stderr
    assert used == fast, (label, 'saved status used' if used else 'full comparison', quick.stderr)
    checks += 1
    print('PASS', label, flush=True)


try:
    src = work_root / 'src'
    for i in range(300):
        f = src / f'd{i % 5}' / f'sub{i % 3}' / f'f{i}'
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_text(f'file {i}\n')
    (src / '.gitignore').write_text('build/\n*.log\n')
    (src / 'tool.sh').write_text('#!/bin/sh\n')
    os.symlink('d0/sub0/f0', src / 'link')
    run(work_root, 'init', 'p', '--from', src, '-q')
    main = work_root / 'store/projects/p/lines/main'
    run(main, 'protect', 'main', '--remove')
    run(main, 'line', 'new', '-q', 'topic')
    work = main.parent / 'topic'
    same('first status compares all', fast=False)
    same('nothing changed')

    (work / 'd0/sub0/f0').write_text('changed\n')
    same('a changed file')
    (work / 'd1/sub1/f1').unlink()
    same('a deleted file')
    (work / 'new.txt').write_text('new\n')
    same('a new file')
    os.rename(work / 'd2/sub2/f2', work / 'd2/sub2/moved')
    same('a renamed file')
    (work / 'tool.sh').chmod(0o755)
    same('an executable bit')
    (work / 'link').unlink()
    os.symlink('d0/sub0/f5', work / 'link')
    same('a changed link')
    (work / 'build/out').mkdir(parents=True)
    (work / 'build/out/a.o').write_text('object\n')
    (work / 'x.log').write_text('log\n')
    same('ignored files')
    os.rename(work / 'd3', work / 'd3-renamed')
    same('a renamed folder')
    shutil.rmtree(work / 'd4/sub1')
    same('a deleted folder')
    (work / 'fresh/deep').mkdir(parents=True)
    (work / 'fresh/deep/file').write_text('deep\n')
    same('a new folder with content')
    (work / 'd0/sub0/f0').write_text('file 0\n')
    same('a file changed back')
    os.utime(work / 'd0/sub0/f15', ns=(0, 10**18))
    same('only a new time')
    run(work, 'add', 'new.txt', 'd0/sub0/f0')
    same('status after add compares all', fast=False)
    (work / 'new.txt').write_text('new and changed\n')
    same('a staged and changed file')
    (work / '.gitignore').write_text('build/\n')
    same('a changed .gitignore compares all', fast=False)
    run(work, 'add', '-A')
    run(work, 'save', '-qm', 'all')
    same('status after commit compares all', fast=False)
    same('clean after commit')
    # Two names of one file inside the line: a write through one name changes both.
    os.link(work / 'd0/sub1/f10', work / 'second-name')
    same('a new hard link in the line')
    with open(work / 'second-name', 'ab') as f:
        f.write(b'through the second name\n')
    same('a write through the second name')
    (work / 'second-name').unlink()
    same('the second name is gone')
    # A write through a name outside the line changes the shared inode.
    outside = work_root / 'outside-name'
    os.link(work / 'd0/sub0/f15', outside)
    same('a hard link outside the line')
    with open(outside, 'r+b') as f:
        f.write(b'X')
    same('a write through the outside name')
    os.unlink(outside)
    for i in range(40):
        (work / f'd1/sub{i % 3}/many{i}').write_text(f'again {i}\n')
    same('many changed files')
    # A write through a memory mapping changes the change counter. While a file is mapped it
    # has no counter, so status compares all.
    with (work / 'd1/sub1/f16').open('r+b') as f:
        with mmap.mmap(f.fileno(), 0) as m:
            m[0:1] = b'Z'
            m.flush()
            # A mapped file has no change counter: it is compared every time.
            same('a mapped file is compared each time')
            m[2:3] = b'W'
            m.flush()
            same('a second write through the mapping')
    same('after the mapping ends')
    with (work / 'd1/sub1/f16').open('r+b') as f:
        with mmap.mmap(f.fileno(), 0) as m:
            m[1:2] = b'Y'
            m.flush()
    same('a write through a closed mapping')
    assert 'd1/sub1/f16' in run(work, 'status', '--porcelain').stdout
    print(f'{checks} status comparisons passed', flush=True)
finally:
    subprocess.run(['chflags', '-R', 'nouchg', str(work_root)], check=False)
    shutil.rmtree(work_root, ignore_errors=True)
