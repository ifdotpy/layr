#!/usr/bin/env python3
"""Save compares each immutable pair once and retains complete object write evidence."""
import errno
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root, binary = Path(sys.argv[1]), str(Path(sys.argv[2]).resolve())
source = root / 'source'
source.mkdir()
for i in range(16):
    (source / f'f{i:02}').write_text(f'original {i}\n')
(source / 'directory').mkdir()
(source / 'directory/untouched').write_text('untouched\n')
project = Path(os.environ['LAYR_ROOT']) / 'projects/p'
work = project / 'lines/main'
env = dict(os.environ, LAYR_DIRECT='1', LAYR_SOCKET='/nonexistent/save-reads.sock')
for key in ('LAYR_PROJECT', 'LAYR_LINE', 'LAYR_TRACE', 'LAYR_NO_STATUS_CACHE'):
    env.pop(key, None)


def run(*args, cwd=None, trace=False, no_cache=False):
    extra = {'LAYR_TRACE': '1'} if trace else {}
    if no_cache:
        extra['LAYR_NO_STATUS_CACHE'] = '1'
    result = subprocess.run([binary, *args], cwd=cwd or work, env=dict(env, **extra),
                            capture_output=True, text=True, timeout=60)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result


run('init', 'p', '--no-objects', '--from', str(source), '-q', cwd=root)
run('protect', 'main', '--remove')
# A disabled, cold or invalid cache must never turn the warning into two full reads.
for flags in ([], ['-q'], ['--json']):
    with (work / 'f00').open('a') as output:
        output.write('staged\n')
    run('add', 'f00')
    (work / 'f01').write_text('left out\n')
    result = run('save', '-m', 'native staged save', *flags, trace=True, no_cache=True)
    assert result.stderr.count('status full comparison') == 1, result.stderr
    assert 'not saved (modified, not added): f01' in result.stderr, result.stderr
    if '--json' in flags:
        json.loads(result.stdout)

# A named save compares the head with the index only where `add` staged: not the whole tree.
# Another writer of the index (`rm --cached`) makes it compare everything again.
for i in range(40):
    (work / f'many{i:02}').write_text(f'many {i}\n')
run('add', '-A')
run('save', '-m', 'many files')
time.sleep(1.1)  # Index entries younger than a second are compared again (racy).
with (work / 'many00').open('a') as output:
    output.write('staged\n')
run('add', 'many00')
result = run('save', '-m', 'one staged file', trace=True, no_cache=True)
tracked = len(run('ls-files').stdout.split())
# The warning compares every tracked file once; the staged comparison only `many00`. A copied
# index (overlayfs) has new change times: all its entries are racy right after `add`.
if not os.environ.get('LAYR_OVERLAYFS_TEST_ROOT'):
    assert result.stderr.count('content compare') <= tracked + 1, (tracked, result.stderr.count('content compare'))
for name in ('many01', 'many02', 'many04'):
    with (work / name).open('a') as output:
        output.write('changed\n')
run('add', 'many01')
run('add', 'many04')
run('rm', '--cached', '-q', 'many03')
saved = json.loads(run('save', '-m', 'staged by add and rm', '--json').stdout)
# The comparison decides the checks and the summary of the save.
assert saved['files'] == {'changed': 3, 'insertions': 2, 'deletions': 1}, saved
assert run('show', 'head:many01').stdout.endswith('changed\n')
assert run('show', 'head:many04').stdout.endswith('changed\n')
assert not run('show', 'head:many02').stdout.endswith('changed\n')
assert 'many03' not in run('ls-files').stdout.split()
# Another writer (an older Layr) changes the index after the record of `add`: the record no
# longer describes it, and the save compares everything.
run('add', 'many05')
[stage] = [path for path in (project / 'meta/stage').iterdir() if path.is_dir()]
(stage / 'many06').write_text('foreign\n')
saved = json.loads(run('save', '-m', 'foreign index change', '--json').stdout)
assert saved['files'] == {'changed': 1, 'insertions': 1, 'deletions': 1}, saved

# Retire the native base so the automatic save compares objects against its immutable capture.
try:
    work.rename(root / 'hidden')
    hidden = True
except OSError as error:
    if error.errno not in (errno.EXDEV, errno.EBUSY):
        raise
    hidden = False
try:
    run('--project', 'p', 'migrate', cwd=root)
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for path in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(path), 'ro', 'false'], check=True)
    run('--project', 'p', 'migrate', '--delete-old', cwd=root)
finally:
    if hidden:
        (root / 'hidden').rename(work)

# A late content change makes equality visit all preceding equal files. The checked
# delta must also retain a touched file, dirty directory and changed hard-link group.
(work / 'f15').write_text('automatic content change\n')
os.chmod(work / 'f02', 0o755)
os.utime(work / 'f03', ns=(1_600_000_000_000_000_001, 1_600_000_000_000_000_001))
os.utime(work / 'directory', ns=(1_600_000_000_000_000_002, 1_600_000_000_000_000_002))
os.link(work / 'f04', work / 'linked')
result = run('save', '--json', trace=True)
assert 'write checked objects' in result.stderr, result.stderr
assert 'objects full comparison' not in result.stderr, result.stderr
assert result.stderr.count('compare file hash f00') == 1, result.stderr
saved = json.loads(result.stdout)
assert saved['saved'], saved
checkout = root / 'automatic-checkout'
run('objects', 'checkout', saved['state'], str(checkout))
for name in ('f02', 'f03', 'f04', 'f15', 'directory', 'directory/untouched', 'linked'):
    before, after = (work / name).stat(), (checkout / name).stat()
    assert before.st_mode & 0o7777 == after.st_mode & 0o7777, name
    assert before.st_mtime_ns == after.st_mtime_ns, name
    if (work / name).is_file():
        assert (work / name).read_bytes() == (checkout / name).read_bytes(), name
assert (checkout / 'f04').stat().st_ino == (checkout / 'linked').stat().st_ino
forced = json.loads(run('save', '--force', '--json').stdout)
assert forced['saved'] and forced['state'] != saved['state'], forced
print('SAVE READS PASS')
