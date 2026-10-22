#!/usr/bin/env python3
"""Compare status from FSEvents with status from a full read, after many kinds of changes.

The test makes and attaches its own small case-sensitive APFS image without -nobrowse: macOS
keeps an FSEvents log only on such volumes. It removes the image at the end.
"""
import argparse, mmap, os, shutil, subprocess, tempfile, time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--root', required=True, help='an empty folder for the image (any volume)')
a = p.parse_args()
binary = str(Path(a.binary).resolve())
work_root = Path(tempfile.mkdtemp(prefix='apfs-watch-', dir=Path(a.root).resolve()))
image = work_root / 'watch.sparseimage'
mount = work_root / 'mnt'
mount.mkdir()
subprocess.run(['hdiutil', 'create', '-size', '1g', '-type', 'SPARSE', '-fs', 'Case-sensitive APFS',
                '-volname', 'LayrWatch', str(image)], check=True, capture_output=True)
subprocess.run(['hdiutil', 'attach', '-mountpoint', str(mount), str(image)], check=True, capture_output=True)
env = dict(os.environ, LAYR_ROOT=str(mount / 'store'), LAYR_AUTHOR_NAME='Test', LAYR_AUTHOR_EMAIL='test@example.invalid')
for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE', 'LAYR_TRACE', 'LAYR_NO_WATCH'):
    env.pop(key, None)


def run(cwd, *args, extra=None):
    r = subprocess.run([binary, *map(str, args)], cwd=cwd, env=dict(env, **(extra or {})), capture_output=True, text=True)
    assert r.returncode == 0, (args, r.returncode, r.stdout, r.stderr)
    return r


checks = 0


def same(label, fast=True):
    """The FSEvents status equals the full status; `fast` says the FSEvents path answered."""
    global checks
    quick = run(work, 'status', '--porcelain', '--ignored', extra={'LAYR_TRACE': '1'})
    full = run(work, 'status', '--porcelain', '--ignored', extra={'LAYR_NO_WATCH': '1'})
    assert quick.stdout == full.stdout, (label, quick.stdout, full.stdout)
    used = 'FSEvents status used' in quick.stderr
    assert used == fast, (label, 'FSEvents path used' if used else 'full read', quick.stderr)
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
    main = mount / 'store/projects/p/lines/main'
    run(main, 'protect', 'main', '--remove')
    run(main, 'branch', '-q', 'topic')
    work = main.parent / 'topic'
    same('first status reads all', fast=False)
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
    same('status after add reads all', fast=False)
    (work / 'new.txt').write_text('new and changed\n')
    same('a staged and changed file')
    (work / '.gitignore').write_text('build/\n')
    same('a changed .gitignore reads all', fast=False)
    run(work, 'add', '-A')
    run(work, 'commit', '-qm', 'all')
    same('status after commit reads all', fast=False)
    same('clean after commit')
    os.link(work / 'd0/sub0/f15', work / 'second-name')
    same('a new hard link reads all', fast=False)
    same('a folder with hard links keeps reading all', fast=False)
    (work / 'second-name').unlink()
    same('the link is gone: one full read', fast=False)
    same('then FSEvents again')
    for i in range(40):
        (work / f'd1/sub{i % 3}/many{i}').write_text(f'again {i}\n')
    same('many changed files')
    # A write through a memory mapping gives no event. The saved status serves only for
    # watch.full_seconds after a full read; then a full read shows the write.
    run(work, 'config', 'watch.full_seconds', '2')
    time.sleep(2.2)
    same('a saved status older than watch.full_seconds reads all', fast=False)
    with (work / 'd1/sub1/f16').open('r+b') as f:
        with mmap.mmap(f.fileno(), 0) as m:
            m[0:1] = b'Z'
            m.flush()
    time.sleep(2.2)
    same('a mapped write shows after watch.full_seconds', fast=False)
    assert 'd1/sub1/f16' in run(work, 'status', '--porcelain').stdout
    print(f'{checks} status comparisons passed', flush=True)
finally:
    subprocess.run(['hdiutil', 'detach', '-force', str(mount)], capture_output=True)
    shutil.rmtree(work_root, ignore_errors=True)
