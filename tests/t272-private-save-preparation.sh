#!/bin/bash
# ci: macos macos-plain overlayfs
# Private automatic and final saves prepare both stores before publishing either one.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo public > "$T/src/file"
l init private-prep --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/private-prep
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null
l overlay create second --hidden -q || exit 1
l overlay create pro -q || exit 1
l overlay use pro -q || exit 1
l line new other main -q || exit 1
python3 - "$T" "$LAYR" "$P" <<'PY'
import fcntl
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main, overlay = project / 'lines/main', project / 'meta/overlays/pro'

def run(*args, cwd=main):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result

def rows(store, query):
    with sqlite3.connect(store / 'meta/db/layr.sqlite') as db:
        return db.execute(query).fetchall()

def states(store):
    return set(rows(store, 'SELECT id FROM states'))

def wait_ready(proc, gate):
    deadline = time.monotonic() + 20
    while not (gate / 'ready').exists():
        assert proc.poll() is None, (root / f'{gate.name}.log').read_text()
        assert time.monotonic() < deadline, gate
        time.sleep(.01)

def auto_keys():
    return rows(overlay, "SELECT key, value FROM config WHERE key LIKE 'auto.%' ORDER BY key")

cases = ['content', 'busy', 'final']
if sys.platform == 'darwin':
    cases.insert(2, 'recovery')
for case in cases:
    final = case == 'final'
    line = 'other' if final else 'main'
    working = project / 'lines' / line
    (working / 'file').write_text('private edit')
    (working / 'secret').write_text('private new file')
    before = {store: states(store) for store in (project, overlay)}
    keys = auto_keys()
    gate, retry = root / f'publish-{case}', root / f'retry-{case}'
    gate.mkdir()
    retry.mkdir()
    args = ['line', 'delete', '-D', '--discard', line] if final else ['save', '-q']
    phase = 'CACHE_PARTS' if final else 'PUBLISH'
    env = dict(os.environ, **{f'LAYR_TEST_SAVE_{phase}_BARRIER': str(gate)},
               LAYR_TEST_SAVE_RETRY_BARRIER=str(retry))
    with (root / f'{gate.name}.log').open('w') as log:
        proc = subprocess.Popen([binary, *args], cwd=main, env=env, stdout=log, stderr=log)
    try:
        wait_ready(proc, gate)
        for store in (project, overlay):
            assert states(store) == before[store], 'preparation recorded a state'
            assert {p.name for p in (store / 'states').iterdir()} <= {s[0] for s in before[store]}, 'premature native state'
            assert list((store / 'meta/work').glob('*/tree')), 'capture must live in a workspace'
        assert auto_keys() == keys, 'preparation committed a private mapping'
        # The project lock and the unrelated line remain available during preparation.
        run('tag', f'prepared-{case}', 'head')
        if case == 'content':
            # A private-only edit while publication waits must retry with both stores untouched.
            (working / 'file').write_text('newer private edit')
        if case == 'busy':
            # A second overlay becomes busy after capture. APFS would wait for this
            # pending commit if view validation opened it through blocking recovery.
            second = project / 'meta/overlays/second'
            overlay_lock = (second / 'meta/lock').open('w')
            fcntl.flock(overlay_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            pending = second / 'meta/pending/publication-test'
            pending.mkdir(parents=True)
        if case == 'recovery':
            # An unlocked, abandoned preparation must not be recovered by publication.
            # Its next setup pass recovers it only after releasing the parent lock.
            dead = subprocess.Popen(['true'])
            dead.wait()
            pending = project / f'meta/overlays/second/meta/pending/abandoned.{dead.pid}'
            pending.mkdir(parents=True)
            (pending / 'untouched').write_text('recovery belongs outside the parent lock')
        (gate / 'release').touch()
        if not final:
            wait_ready(proc, retry)
            assert all(states(store) == before[store] for store in (project, overlay))
            assert auto_keys() == keys
            with (project / 'meta/lock').open('w') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if case == 'busy':
                pending.rmdir()
                overlay_lock.close()
            if case == 'recovery':
                assert (pending / 'untouched').is_file(), 'publication performed APFS recovery'
            (retry / 'release').touch()
        assert proc.wait(timeout=20) == 0, (root / f'{gate.name}.log').read_text()
        if case == 'recovery':
            assert not pending.exists(), 'setup did not recover the abandoned preparation'
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
    new = states(project) - before[project]
    assert len(new) == 1, new
    public = next(iter(new))[0]
    assert run('show', f'{public}:file').stdout == 'public\n'
    assert len(states(overlay) - before[overlay]) == 1
    mapped = dict(auto_keys())[f'auto.{public}']
    private = json.loads(mapped)
    expected = 'newer private edit' if case == 'content' else 'private edit'
    assert run('overlay', 'show', 'pro', f'{private}:file').stdout == expected
    assert run('overlay', 'show', 'pro', f'{private}:secret').stdout == 'private new file'
    if not final:
        # Equal private content does not make a new state; a private-only edit does.
        before_equal = {store: states(store) for store in (project, overlay)}
        run('save', '-q')
        assert all(states(store) == before_equal[store] for store in (project, overlay))
        (working / 'file').write_text('private edit after save')
        run('save', '-q')
        assert all(len(states(store) - before_equal[store]) == 1 for store in (project, overlay))
print('private automatic and final saves prepare, retry, compare and publish both stores', flush=True)
PY
ok "private saves use the preparation protocol" test "$?" = 0
finish
