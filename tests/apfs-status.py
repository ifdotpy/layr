#!/usr/bin/env python3
"""Compare APFS status with the snapshot implementation, including live mmap."""
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
env = dict(os.environ, LAYR_ROOT=str(root / 'store'), LAYR_TRACE='1',
           LAYR_AUTHOR_NAME='Test', LAYR_AUTHOR_EMAIL='test@example.invalid')
for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE'):
    env.pop(key, None)


def run(cwd, *args, executable=binary):
    r = subprocess.run([executable, *args], cwd=cwd, env=env, capture_output=True, text=True)
    assert r.returncode == 0, (args, r.returncode, r.stdout, r.stderr)
    return r


checks = 0

def same_status(label, *, fallback=False):
    global checks
    before = run(work, 'status', '--porcelain', '--ignored', executable=baseline)
    after = run(work, 'status', '--porcelain', '--ignored')
    assert before.stdout == after.stdout, (label, before.stdout, after.stdout)
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
    run(root, 'init', 'test', '--from', str(source), '-q')
    main = root / 'store/projects/test/lines/main'
    run(main, 'branch', '-q', 'topic')
    work = main.parent / 'topic'
    assert not same_status('clean status')

    old = (work / 'file').stat()
    with (work / 'file').open('r+b') as f:
        f.seek(8192)
        f.write(b'x')
    os.utime(work / 'file', ns=(old.st_atime_ns, old.st_mtime_ns))
    assert ' M file' in same_status('equal size and restored mtime')
    run(work, 'add', 'file')
    assert 'M  file' in same_status('staged file')
    with (work / 'file').open('r+b') as f:
        f.write(b'y')
    assert 'MM file' in same_status('staged and unstaged bytes')

    old = (work / 'atomic').stat()
    (work / 'replacement').write_text('new\n')
    os.utime(work / 'replacement', ns=(old.st_atime_ns, old.st_mtime_ns))
    os.replace(work / 'replacement', work / 'atomic')
    assert ' M atomic' in same_status('atomic replacement with restored mtime')
    (work / 'mode').chmod(0o755)
    (work / 'remove').unlink()
    (work / 'link').unlink()
    (work / 'link').symlink_to('atomic')
    (work / 'ignored').mkdir()
    (work / 'ignored/data').write_text('ignored\n')
    (work / 'new-dir').mkdir()
    (work / 'new-dir/новый').write_text('new\n')
    same_status('mode, deletion, links, ignored and Unicode paths')

    (work / 'readonly').chmod(0o644)
    (work / 'readonly').write_text('new\n')
    (work / 'readonly').chmod(0o444)
    run(work, 'add', '-A')
    same_status('index replacement of read-only files')
    run(work, 'commit', '-qm', 'changes')
    assert not run(work, 'status', '--porcelain').stdout.strip()
    assert (work / 'readonly').stat().st_mode & 0o777 == 0o444
    assert (main / 'readonly').read_text() == 'old\n'
    run(work, 'fsck')

    with (work / 'file').open('r+b') as f:
        with mmap.mmap(f.fileno(), 0) as mapped:
            mapped[4096:4097] = b'z'
            mapped.flush()
            assert ' M file' in same_status('live mmap uses snapshot fallback', fallback=True)
    same_status('unmapped file returns to validated read')
    assert not list((root / 'store').rglob('.layr-copy-*'))
    print(f'{checks} status comparisons passed', flush=True)
finally:
    subprocess.run(['chflags', '-R', 'nouchg', str(root)], check=True)
    import shutil
    shutil.rmtree(root)
