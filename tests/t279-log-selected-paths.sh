#!/bin/bash
# ci: macos overlayfs
# Literal history selections read both sides, and unavailable pairs do not end traversal.
. "$(dirname "$0")/lib.sh"

python3 - "$T" "$LAYR" <<'PY'
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

root, binary = Path(sys.argv[1]), sys.argv[2]

def run(*args, cwd=root):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout.strip()

for objects in (False, True):
    name = 'portable' if objects else 'native'
    source = root / name
    (source / 'nested').mkdir(parents=True)
    (source / 'nested/file.txt').write_text('initial\n')
    (source / 'other').mkdir()
    (source / 'other/untouched').write_text('unrelated\n')
    args = ('--objects',) if objects else ('--no-objects',)
    run('init', name, *args, '--from', str(source), '-q')
    project = Path(os.environ['LAYR_ROOT']) / 'projects' / name
    work = project / 'lines/main'
    run('protect', 'main', '--remove', cwd=work)
    initial = run('log', '--format=%s', cwd=work)
    if not objects:
        run('line', 'new', 'keep-initial', '-q', cwd=work)

    def save(message):
        run('save', '-qam', message, cwd=work)

    def history(*paths, cwd=work):
        return run('log', '--format=%s', '--', *paths, cwd=cwd).splitlines()

    (work / 'nested/file.txt').write_text('selected\n')
    save('selected edit')
    if not objects:
        run('line', 'new', 'keep-selected', '-q', cwd=work)
    (work / 'other/untouched').write_text('unrelated edit\n')
    save('unrelated edit')
    assert history('nested/file.txt') == ['selected edit', initial]
    assert history('file.txt', cwd=work / 'nested') == ['selected edit', initial]
    assert history('nested', 'nested/file.txt', 'nested') == ['selected edit', initial]
    assert history('nested/*.txt') == ['selected edit', initial]
    assert history('absent') == []
    assert history('.') == ['unrelated edit', 'selected edit', initial]

    # Evict a native parent through Layr instead of moving sealed snapshots or mounts.
    # The latest state uses objects so an APFS thin layer cannot keep its native parent.
    # The two extra line heads keep the older comparison pairs readable on every backend.
    if not objects:
        parent = json.loads(run('log', '-1', '--json', cwd=work))['id']
        run('config', 'objects.enabled', 'true', cwd=work)
        (work / 'nested/file.txt').write_text('latest\n')
        save('latest edit')
        run('remote', 'add', 'archive', (root / 'archive').as_uri(),
            '--paper-key-out', str(root / 'paper'), cwd=work)
        run('push', 'archive', '-q', cwd=work)
        run('config', 'undo.ops', '0', cwd=work)
        run('config', 'undo.deleted', '0', cwd=work)
        run('config', 'gc.evict', 'on', cwd=work)
        if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
            for state in (project / 'states').iterdir():
                subprocess.run(['btrfs', 'property', 'set', '-ts', str(state), 'ro', 'false'], check=True)
        run('gc', '--repair', '-q', cwd=work)
        assert not any((project / kind / parent).exists() for kind in ('states', 'deltas'))
        with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
            assert db.execute('SELECT present,deleted FROM states WHERE id=?', (parent,)).fetchone() == (0, 0)
        selected = history('nested/file.txt')
        assert selected == ['selected edit', initial], selected
        assert not any((project / kind / parent).exists() for kind in ('states', 'deltas'))
        assert run('log', '--format=%s', cwd=work).splitlines() == [
            'latest edit', 'unrelated edit', 'selected edit', initial]
        run('config', 'gc.evict', 'off', cwd=work)
        assert run('show', parent + ':other/untouched', cwd=work) == 'unrelated edit'
        assert history('nested/file.txt') == ['latest edit', 'selected edit', initial]
        run('config', 'objects.enabled', 'false', cwd=work)

    (work / 'nested/file.txt').rename(work / 'renamed.txt')
    run('add', 'nested/file.txt', 'renamed.txt', cwd=work)
    save('rename')
    assert history('nested/file.txt')[0] == 'rename'
    assert history('renamed.txt') == ['rename'], history('renamed.txt')
    (work / 'renamed.txt').unlink()
    save('delete')
    assert history('renamed.txt') == ['delete', 'rename']
    (work / 'renamed.txt').mkdir()
    (work / 'renamed.txt/child').write_text('new\n')
    run('add', 'renamed.txt', cwd=work)
    save('recreate directory')
    assert history('renamed.txt')[0] == 'recreate directory'
    (work / 'link').symlink_to('before')
    run('add', 'link', cwd=work)
    save('add symlink')
    (work / 'link').unlink()
    (work / 'link').symlink_to('after')
    save('change symlink')
    assert history('link') == ['change symlink', 'add symlink']
    assert history('link/child') == []
    print(f'{name}: literal, wildcard, directory, rename, delete and symlink history passed')
PY
eq 'selected history preserves semantics and skips unavailable comparisons' 0 "$?"
finish
