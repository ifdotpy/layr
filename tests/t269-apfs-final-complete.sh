#!/bin/bash
# ci: macos macos-plain overlayfs
# APFS final saves prepare complete storage; final keyframe workers never copy a missing view.
if [ "$(uname -s)" != Darwin ]; then
    echo "t269-apfs-final-complete.sh (APFS only, skipped): 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo initial > "$T/src/file"
l init complete-final --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/complete-final
cd "$P/lines/main"
python3 - "$T" "$LAYR" "$P" <<'PY'
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main = project / 'lines/main'
def run(*args, cwd=main, env=None, timeout=20):
    result = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result
for changed in (False, True):
    name = 'changed' if changed else 'unchanged'
    run('line', 'new', '-q', name, 'main')
    work = project / 'lines' / name
    (work / 'file').write_text('saved thin state')
    run('save', '-q', cwd=work)
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        previous = db.execute("SELECT id FROM states WHERE kind='auto' ORDER BY time DESC,id DESC LIMIT 1").fetchone()[0]
    assert (project / 'deltas' / previous).exists()
    assert not (project / 'states' / previous).exists()
    # A final worker must defer instead of materializing this view under its project lock.
    run('__keyframe', os.environ['LAYR_ROOT'], 'complete-final', previous, 'final',
        env=dict(os.environ, LAYR_TRACE='1'), timeout=3)
    assert not (project / 'states' / previous).exists()
    if changed:
        (work / 'file').write_text('final changed bytes')
    barrier = root / name
    barrier.mkdir()
    with (root / f'{name}.log').open('w') as log:
        deleter = subprocess.Popen([binary, 'line', 'delete', '-D', '--discard', name], cwd=main,
                                  env=dict(os.environ, LAYR_TEST_SAVE_FINAL_COMPLETE_BARRIER=str(barrier),
                                           LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1'), stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 20
        while not (barrier / 'ready').exists():
            assert deleter.poll() is None, (root / f'{name}.log').read_text()
            assert time.monotonic() < deadline
            time.sleep(.01)
        assert any((p / 'tree').exists() for p in (project / 'meta/work').iterdir()), 'no creator-owned complete capture'
        run('tag', f'during-{name}', 'head', timeout=3)
        if changed:
            (work / 'file').write_text('late at final_complete')
        (barrier / 'release').touch()
        assert deleter.wait(timeout=20) == 0, (root / f'{name}.log').read_text()
        with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
            data = json.loads(db.execute("SELECT raw FROM records WHERE op='line.delete' ORDER BY time DESC,seq DESC LIMIT 1").fetchone()[0])['r']['data']
        final = data['final_state']
        assert (project / 'states' / final).is_dir(), 'final complete tree was not published by rename'
        if not changed:
            assert final == previous, 'unchanged final save must retain its exact state ID'
        expected = 'late at final_complete' if changed else 'saved thin state'
        assert (project / 'states' / final / 'file').read_text() == expected
    finally:
        if deleter.poll() is None:
            deleter.kill()
            deleter.wait()
# Hard-linked live content requires the complete-capture fallback and must survive undo.
run('line', 'new', '-q', 'linked-final', 'main')
linked = project / 'lines/linked-final'
(linked / 'file').write_text('linked final bytes')
os.link(linked / 'file', linked / 'same-file')
run('line', 'delete', '-D', '--discard', 'linked-final')
run('undo')
assert (linked / 'file').read_text() == 'linked final bytes'
assert (linked / 'file').stat().st_ino == (linked / 'same-file').stat().st_ino
# CI cannot load the signed LayrFS extension; run the virtual case when it is available.
extensions = subprocess.run(['pluginkit', '-m', '-p', 'com.apple.fskit.fsmodule'], capture_output=True, text=True)
if 'dev.layr.fshost.fs' in extensions.stdout:
    virtual_env = dict(os.environ, LAYR_EXPERIMENTAL_VIEWS='1', LAYR_FS_SOCKET=str(root / 'fs.sock'),
                       LAYR_FS_STRICT='1', LAYR_FS_NO_WATCH='')
    run('line', 'new', '-q', 'virtual-final', 'main', '--view', 'virtual', env=virtual_env)
    (project / 'lines/virtual-final/new-file').write_text('virtual final bytes')
    run('line', 'delete', '-D', '--discard', 'virtual-final', env=virtual_env)
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        final = json.loads(db.execute("SELECT raw FROM records WHERE op='line.delete' ORDER BY time DESC,seq DESC LIMIT 1").fetchone()[0])['r']['data']['final_state']
    assert (project / 'states' / final / 'new-file').read_text() == 'virtual final bytes'
    run('fs', 'stop', env=virtual_env)
else:
    print('virtual final case skipped: signed LayrFS extension unavailable', flush=True)
print('APFS changed/unchanged, late-write, hard-link and virtual final saves preserve complete content', flush=True)
PY
ok "APFS finals never keyframe by copying under the project lock" test "$?" = 0
finish
