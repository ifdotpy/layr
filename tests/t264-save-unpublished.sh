#!/bin/bash
# ci: macos macos-plain overlayfs
# A preparation survives repair/prune and a crash exposes no partial state.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo initial > "$T/src/file"
echo target/ > "$T/src/.gitignore"
l init unpublished --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/unpublished
cd "$P/lines/main"
l protect main --remove >/dev/null
l migrate >/dev/null || exit 1
python3 - "$T" "$LAYR" "$P" <<'PY'
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main, db_path = project / 'lines/main', project / 'meta/db/layr.sqlite'
def run(*args, timeout=20):
    result = subprocess.run([binary, *args], cwd=main, capture_output=True, text=True, timeout=timeout)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result
def objects():
    return {p for area in ('objects', 'blobs')
            for p in (project / 'meta/objects' / area).rglob('*') if p.is_file()}
def states():
    with sqlite3.connect(db_path) as db:
        return {row[0] for row in db.execute('SELECT id FROM states')}
for crash in (False, True):
    barrier = root / ('crash' if crash else 'repair')
    barrier.mkdir()
    (main / 'file').write_text(barrier.name)
    before = states()
    work_before = set((project / 'meta/work').iterdir())
    objects_before = objects()
    with (root / f'{barrier.name}.log').open('w') as log:
        saver = subprocess.Popen([binary, 'save', '-q'], cwd=main,
                                 env=dict(os.environ, LAYR_TEST_SAVE_PUBLISH_BARRIER=str(barrier)), stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 20
        while not (barrier / 'ready').exists():
            assert saver.poll() is None, (root / f'{barrier.name}.log').read_text()
            assert time.monotonic() < deadline
            time.sleep(.01)
        assert states() == before, 'preparation records no partial state'
        abandoned = set((project / 'meta/work').iterdir()) - work_before
        unrecorded_objects = objects() - objects_before
        assert abandoned and unrecorded_objects, 'the blocked save owns work and newly encoded objects'
        native = {p.name for p in (project / 'states').iterdir()}
        assert native <= before, 'no unrecorded states/<id> capture'
        run('fsck', '--repair')
        run('gc')
        assert states() == before
        if crash:
            saver.kill()
            saver.wait()
            run('status', '--short')
            assert states() == before, 'crash before publication records nothing'
            run('gc', '--repair')
            assert all(not p.exists() for p in abandoned), 'dead creator work was not reclaimed'
            assert all(not p.exists() for p in unrecorded_objects), 'unpublished objects were not pruned'
            run('save', '-q')
        else:
            (barrier / 'release').touch()
            assert saver.wait(timeout=20) == 0, (root / f'{barrier.name}.log').read_text()
        assert len(states() - before) == 1
        run('fsck')
    finally:
        if saver.poll() is None:
            saver.kill()
            saver.wait()
print('repair, prune and crash preserve preparation ownership', flush=True)
PY
ok "unpublished captures survive repair and crash safely" test "$?" = 0
if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
python3 - "$T" "$LAYR" "$TG" <<'PY'
import fcntl
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary = Path(sys.argv[1]), sys.argv[2]
store = root / 'backend-store'
env = dict(os.environ, LAYR_ROOT=str(store), LAYR_TEST_PERIODIC='1')
project, registry = store / 'projects/backend', store / '.layr-overlayfs'
main = project / 'lines/main'
def run(*args, success=True, cwd=main):
    r = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=20)
    assert (r.returncode == 0) == success, (args, r.stdout, r.stderr)
    return r
def start(name, *args, **hooks):
    with (root / f'{name}.log').open('w') as log:
        return subprocess.Popen([binary, *args], cwd=main, env=dict(env, **hooks), stdout=log, stderr=log)
def wait_ready(proc, barrier):
    end = time.monotonic() + 20
    while not (barrier / 'ready').exists():
        assert proc.poll() is None, f'{barrier}: process exited {proc.returncode}'
        assert time.monotonic() < end, barrier
        time.sleep(.01)
def gate(name):
    path = root / name
    path.mkdir()
    return path
def tag_waiting(name):
    barrier = gate(name)
    # lib.sh normally empties trash synchronously after unlocking. That cleanup needs
    # the registry too; this probe measures only the tag's project-lock operation.
    proc = start(name, 'tag', name, 'head', LAYR_TEST_SAVE_PROJECT_LOCK_BARRIER=str(barrier),
                 LAYR_TRASH_BUDGET_MS='0')
    wait_ready(proc, barrier) # Store recovery is already past the registry lock.
    return proc, barrier
def tag_completes(proc, barrier):
    (barrier / 'release').touch()
    assert proc.wait(timeout=3) == 0, (root / f'{barrier.name}.log').read_text()
def parent_available():
    with (project / 'meta/lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
def states():
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        return {r[0] for r in db.execute('SELECT id FROM states')}
run('init', 'backend', '--from', str(root / 'src'), '-q', cwd=root)
run('protect', 'main', '--remove')
run('config', 'regenerable', '["target"]')
(main / 'target').mkdir(exist_ok=True)
(main / 'target/output').write_text('cache')
# Sealing the prepared EMPTY root triggers store-wide cleanup outside the parent lock.
abandoned = registry / 'objects/abandoned.tmp'
abandoned.mkdir()
(abandoned / 'large-output').write_bytes(b'x' * (1024 * 1024))
collect = gate('outside-collect')
tag, tag_barrier = tag_waiting('during-collect')
saver = start('empty-setup', 'save', '--force', '-q', LAYR_TEST_SAVE_OVERLAY_COLLECT_BARRIER=str(collect))
try:
    wait_ready(saver, collect)
    assert abandoned.exists()
    parent_available()
    tag_completes(tag, tag_barrier)
finally:
    (collect / 'release').touch()
assert saver.wait(timeout=20) == 0, (root / 'empty-setup.log').read_text()
assert not abandoned.exists(), 'outside-lock collection did not reclaim the abandoned object'
# Retirement of a removed overlay and stale preparation disposal must not wait for the registry.
for replacement in (False, True):
    if replacement:
        run('overlay', 'remove', sys.argv[3])
    publish, retry = gate(f'publish-{replacement}'), gate(f'retry-{replacement}')
    before = states()
    saver = start(f'stale-{replacement}', 'save', '--force', '-q',
                  **{'LAYR_TEST_SAVE_SETUP_BARRIER' if replacement else 'LAYR_TEST_SAVE_PUBLISH_BARRIER': str(publish),
                     'LAYR_TEST_SAVE_RETRY_BARRIER': str(retry)})
    wait_ready(saver, publish)
    tag, tag_barrier = tag_waiting(f'during-registry-{replacement}')
    with (registry / 'lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        (publish / 'release').touch()
        wait_ready(saver, retry) # Setup refusal or stale disposal finished without a backend wait.
        if replacement:
            assert (project / 'meta/overlays' / sys.argv[3]).is_dir(), 'contended retirement moved the old identity'
        parent_available()
        assert states() == before
        tag_completes(tag, tag_barrier)
    (retry / 'release').touch()
    assert saver.wait(timeout=20) == 0, (root / f'stale-{replacement}.log').read_text()
# A periodic registry collision returns without updating its unchanged-check timestamp.
run('config', 'snapshot.interval', '0')
checks = project / 'meta/periodic-checks.json'
with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
    line = db.execute("SELECT id FROM lines WHERE name='main'").fetchone()[0]
for phase, transient in (('SNAPSHOT', False), ('CACHE', False), ('CACHE', True)):
    if checks.exists():
        previous = json.loads(checks.read_text())
        previous.pop(line, None)
        checks.write_text(json.dumps(previous))
    name = 'backend-periodic-' + phase.lower() + ('-transient' if transient else '')
    barrier = gate(name)
    hooks = {f'LAYR_TEST_SAVE_{phase}_BARRIER': str(barrier)}
    prepared = gate(name + '-prepared') if transient else None
    if prepared:
        hooks['LAYR_TEST_SAVE_CACHE_PARTS_BARRIER'] = str(prepared)
    periodic = start(name, '__test-periodic', str(store), **hooks)
    try:
        wait_ready(periodic, barrier)
        before_checks = checks.read_bytes() if checks.exists() else None
        before = states()
        with (registry / 'lock').open('w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            (barrier / 'release').touch()
            if prepared:
                # Discovery swallowed the collision. Release the registry before publication
                # could try it: only the remembered collision can keep this check due.
                wait_ready(periodic, prepared)
            else:
                assert periodic.wait(timeout=5) == 0, (root / f'{name}.log').read_text()
        if prepared:
            (prepared / 'release').touch()
            assert periodic.wait(timeout=5) == 0, (root / f'{name}.log').read_text()
        parent_available()
        assert states() == before, 'contended preparation published a state'
        assert (checks.read_bytes() if checks.exists() else None) == before_checks
        run('__test-periodic', str(store))
        assert line in json.loads(checks.read_text()), 'next periodic tick did not retry'
    finally:
        if periodic.poll() is None:
            periodic.kill()
            periodic.wait()
# Both failures occur AFTER rename; cleanup may collect only after releasing the parent lock.
for phase in ('INSTALL', 'RECORD'):
    before = states()
    (main / 'file').write_text('failed ' + phase)
    collect = gate('rollback-' + phase)
    tag, tag_barrier = tag_waiting('during-rollback-' + phase)
    saver = start('rollback-' + phase, 'save', '--force', '-q',
                  **{f'LAYR_TEST_SAVE_{phase}_FAIL': '1', 'LAYR_TEST_SAVE_OVERLAY_COLLECT_BARRIER': str(collect)})
    try:
        wait_ready(saver, collect)
        parent_available()
        tag_completes(tag, tag_barrier)
    finally:
        (collect / 'release').touch()
    assert saver.wait(timeout=20) != 0
    assert states() == before
    assert {p.name for p in (project / 'states').iterdir()} <= before, 'rollback left a published partial state'
print('OverlayFS collection, registry contention, stale disposal, periodic skip and rollback yield the parent lock')
PY
ok "OverlayFS preparation/publication and rollback never wait or reclaim under the parent lock" test "$?" = 0
fi
finish
