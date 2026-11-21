"""Exercise shrink boundaries through native/object saves and the existing index."""
import json
import os
from pathlib import Path
import random
import subprocess
import sys

root, binary = Path(sys.argv[1]), sys.argv[2]
# Object chunks are content-defined (16/64/256 KiB min/average/max). 1 MiB is
# four maximum chunks and reaches the cold object's chunk_min_file threshold.
sizes = (32768, 65536, 131072, 1048576)
failures = []
checks = 0


def run(*args, cwd=root):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, timeout=30)
    assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
    return result.stdout


def check(condition, description):
    global checks
    checks += 1
    if not condition:
        failures.append(description)
        print(f'FAIL {description}', flush=True)


def verify(work, expected, revision, label):
    for name, data in expected.items():
        actual = run('show', f'{revision}:{name}', cwd=work)
        check(actual == data, f'{label}/{name}: saved {len(actual)}, expected {len(data)} bytes')
        check((work / name).read_bytes() == data, f'{label}/{name}: working content changed')


for objects in (False, True):
    for mode in ('named', 'restaged', 'automatic'):
        label = f'{"objects" if objects else "native"}-{mode}'
        source = root / f'{label}-source'
        source.mkdir()
        old = {}
        expected = {}
        for size in sizes:
            for replace in (False, True):
                for middle in (False, True):
                    name = f'{size}-{"replace" if replace else "inplace"}-{"middle" if middle else "rewrite"}'
                    before = random.Random(size).randbytes(size + 237)
                    after = before[:size // 2] + before[size // 2 + 237:] if middle else b'a' * size
                    old[name], expected[name] = before, after
                    (source / name).write_bytes(before[:size // 2] if mode == 'restaged' else before)
        run('init', label, '--objects' if objects else '--no-objects', '--from', str(source), '-q')
        project = Path(os.environ['LAYR_ROOT']) / 'projects' / label
        work = project / 'lines/main'
        run('protect', 'main', '--remove', cwd=work)
        head = run('rev-parse', 'head', cwd=work)
        if mode == 'restaged':
            # Grow a committed file, stage that growth, then shrink it again before
            # the named save. The second add must replace the longer staged entry.
            for name, data in old.items():
                assert (work / name).stat().st_size < len(data), name
                (work / name).write_bytes(b'x' + data[1:])
            run('add', '-A', cwd=work)
            for name, data in old.items():
                check(run('show', f':{name}', cwd=work) == b'x' + data[1:], f'{label}/{name}: staged growth differs')
        for name, data in expected.items():
            path = work / name
            inode = path.stat().st_ino
            if '-replace-' in name:
                temporary = work / 'replacement'
                temporary.write_bytes(data)
                temporary.replace(path)
                assert path.stat().st_ino != inode, name
            else:
                path.write_bytes(data)  # O_TRUNC preserves this working inode.
                assert path.stat().st_ino == inode, name
        if mode == 'automatic':
            saved = json.loads(run('save', '--json', cwd=work))
            assert saved['saved'], saved
            verify(work, expected, saved['state'], label)
            check(run('rev-parse', 'head', cwd=work) == head, f'{label}: automatic save moved named head')
            status = run('status', '--short', cwd=work).decode()
            check(all(name in status for name in expected), f'{label}: status lost edits against named head')
            again = json.loads(run('save', '--json', cwd=work))
            check(not again['saved'] and again['state'] == saved['state'], f'{label}: unchanged automatic save changed state')
        run('add', '-A', cwd=work)
        run('save', '-qm', f'Shrink {label}', cwd=work)
        verify(work, expected, 'head', f'{label}-head')
        status = run('status', '--short', cwd=work).decode()
        check(status == '', f'{label}: status after named save: {status!r}')
        if not objects and mode == 'named':
            # No imported parent: a full cold import must exercise chunks, rather
            # than the hot blobs written by normal object-backed saves.
            run('objects', 'import', 'head', '--cold', '--full', cwd=work)
            stat = json.loads(run('objects', 'stat', '--json', cwd=work))
            check(stat['chunked_blobs'] > 0, f'{label}: cold import did not encode chunks')
            checkout = root / 'cold-checkout'
            run('objects', 'checkout', 'head', str(checkout), cwd=work)
            for name, data in expected.items():
                check((checkout / name).read_bytes() == data, f'{label}/{name}: cold object checkout differs')
        print(f'{label}: checked {len(expected)} shrink cases, sizes {sizes}', flush=True)

print(f'{checks} checks, {len(failures)} failures', flush=True)
assert not failures, '\n'.join(failures)
