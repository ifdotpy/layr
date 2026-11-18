#!/bin/bash
# ci: macos macos-plain overlayfs
# Retired heads, cold caches, optional failures and removed-overlay replacement.
. "$(dirname "$0")/lib.sh"
# A missing imported head is still recoverable from its local Git objects during preparation.
mkdir -p "$T/up"
git -C "$T/up" init -q -b main
printf 'original\n' > "$T/up/file"
printf 'target/\n' > "$T/up/.gitignore"
git -C "$T/up" add .
git -C "$T/up" commit -qm initial
l init git-recovery --from "$T/up" -q || exit 1
python3 - "$T" "$LAYR" <<'PY'
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary = Path(sys.argv[1]), sys.argv[2]
project = Path(os.environ['LAYR_ROOT']) / 'projects/git-recovery'
main = project / 'lines/main'
def run(*args, **kwargs):
    result = subprocess.run([binary, *args], cwd=main, capture_output=True, text=True, timeout=20, **kwargs)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result
run('protect', 'main', '--remove')
run('line', 'new', '-q', 'compared', 'main')
# Configuration may read the head and initialize cold caches. Finish it before
# hiding the head so only save preparation can recover the missing input.
run('config', 'regenerable', '["target"]')
with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
    head = db.execute("SELECT head FROM lines WHERE name='main'").fetchone()[0]
native = project / 'states' / head
# States can live on a different volume from the test root. Hide beside the state
# so this fixture only renames, just like publication on that volume.
hidden = native.with_name('.hidden-git-head')
# Hide native storage without losing the imported record or local Git objects.
# OverlayFS needs its mount and registry entry removed together in this isolated fixture.
registry = Path(os.environ['LAYR_ROOT']) / '.layr-overlayfs/registry.json'
if registry.exists():
    import fcntl
    import json
    with registry.with_name('lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        data = json.loads(registry.read_text())
        data['trees'].pop(str(native))
        # The empty native mountpoint remains visible; unmount it before hiding the directory.
        subprocess.run(['umount', str(native)], check=True)
        native.rmdir()
        registry.write_text(json.dumps(data))
else:
    if sys.platform != 'darwin':
        subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True)
    else:
        subprocess.run(['chflags', 'nouchg', str(native)], check=True)
    native.rename(hidden)
try:
    for force in (True, False):
        work = main if force else project / 'lines/compared'
        if force:
            (main / 'target/output').write_text('fresh cache output')
        (work / 'file').write_text('Git recovery forced' if force else 'Git recovery compared')
        barrier = root / ('git-forced' if force else 'git-compared')
        barrier.mkdir()
        with (root / f'{barrier.name}.log').open('w') as log:
            saver = subprocess.Popen([binary, 'save', '-q', *(['--force'] if force else [])], cwd=work,
                env=dict(os.environ, LAYR_TEST_SAVE_GIT_RECOVERY_BARRIER=str(barrier),
                         LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1'), stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 20
            while not (barrier / 'ready').exists():
                assert saver.poll() is None, (root / f'{barrier.name}.log').read_text()
                assert time.monotonic() < deadline, (root / f'{barrier.name}.log').read_text()
                time.sleep(.01)
            assert not native.exists(), 'Git recovery published native storage during prepare'
            run('tag', barrier.name, 'head')
            (barrier / 'release').touch()
            assert saver.wait(timeout=20) == 0, (root / f'{barrier.name}.log').read_text()
            assert not native.exists(), 'Git input should remain operation-owned'
            print(f'Git recovery force={force}: passed under the strict preparation guard', flush=True)
        finally:
            if saver.poll() is None:
                saver.kill()
                saver.wait()
    print('missing Git heads recover before publication, with comparison and cold-cache discovery', flush=True)
finally:
    if hidden.exists():
        hidden.rename(native)
    for work in (main, project / 'lines/compared'):
        git = work / '.git'
        if git.exists():
            git.chmod(0o755)
PY
ok "missing Git heads recover outside the project lock" test "$?" = 0
mkdir -p "$T/src"
echo initial > "$T/src/file"
echo 'target/' > "$T/src/.gitignore"
# Declare the cache after migration so init leaves no saved cache part.
python3 - "$T" "$LAYR" "$TG" <<'PY'
import errno
import fcntl
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

root, binary, overlay = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
project = Path(os.environ['LAYR_ROOT']) / 'projects/recovery'
main = project / 'lines/main'
def run(*args, cwd=main, env=None, timeout=20, success=True):
    result = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
    assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
    return result
run('init', 'recovery', '--from', str(root / 'src'), '-q', cwd=root)
run('protect', 'main', '--remove')
try:
    main.rename(root / 'hidden-main')
    hidden = True
except OSError as error:
    if error.errno not in (errno.EXDEV, errno.EBUSY):
        raise
    hidden = False
try:
    run('--project', 'recovery', 'migrate', cwd=root)
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for state in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(state), 'ro', 'false'], check=True)
    run('--project', 'recovery', 'migrate', '--delete-old', cwd=root)
finally:
    if hidden:
        (root / 'hidden-main').rename(main)
with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
    head = db.execute("SELECT head FROM lines WHERE name='main'").fetchone()[0]
assert not (project / 'states' / head).exists(), 'fixture head must be object-only'
run('config', 'regenerable', '["target"]')
assert (main / 'target').is_dir(), 'config must initialize the cold cache'
(main / 'target/output').write_text('fresh cache output')
for phase in ('PREPARE', 'PUBLISH'):
    (main / 'file').write_text(f'parent with failed cache {phase}')
    result = run('save', '--force', '-q', env=dict(os.environ, LAYR_TRACE='1',
                 LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1', **{f'LAYR_TEST_SAVE_CACHE_{phase}_FAIL': '1'}))
    assert 'warning: cache overlay' in result.stderr and 'injected optional cache failure' in result.stderr, result.stderr
    assert 'project lock acquired during save preparation' not in result.stderr, result.stderr
    assert 'build ' not in result.stderr or 'from objects' not in result.stderr, result.stderr
    assert not (project / 'states' / head).exists(), 'cache discovery rebuilt the retired head'
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        saved = db.execute("SELECT id FROM states WHERE kind='auto' ORDER BY time DESC,id DESC LIMIT 1").fetchone()[0]
    assert f'failed cache {phase}' in run('show', f'{saved}:file').stdout
# Populate the cache, remove it, then verify its old project cannot be replaced while locked.
run('save', '--force', '-q')
old_dir = project / 'meta/overlays' / overlay
with sqlite3.connect(old_dir / 'meta/db/layr.sqlite') as db:
    old_id = db.execute("SELECT json_extract(raw, '$.r.data.id') FROM records WHERE op='project' LIMIT 1").fetchone()[0]
run('overlay', 'remove', overlay)
assert old_dir.exists(), 'removed overlay remains until replacement or GC'
# Ensure the retired project contains enough old data to expose a recursive deletion path.
(old_dir / 'old-output').write_bytes(b'x' * (1024 * 1024))
with (old_dir / 'meta/lock').open('w') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    run('save', '--force', '-q', success=False, timeout=5)
    assert (old_dir / 'old-output').exists(), 'busy overlay was retired despite its lock'
    run('tag', 'replacement-busy', 'head', timeout=3)
result = run('save', '--force', '-q', env=dict(os.environ, LAYR_TRACE='1', LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1'))
assert not any(line.endswith(f'delete {old_dir}') for line in result.stderr.splitlines()), result.stderr
with sqlite3.connect(old_dir / 'meta/db/layr.sqlite') as db:
    new_id = db.execute("SELECT json_extract(raw, '$.r.data.id') FROM records WHERE op='project' LIMIT 1").fetchone()[0]
assert old_id != new_id, 'replacement must have a fresh project identity'
assert not (old_dir / 'old-output').exists()
print('retired-head recovery, optional caches and bounded overlay replacement passed', flush=True)
PY
ok "cache recovery and failures preserve the prepared parent" test "$?" = 0
finish
