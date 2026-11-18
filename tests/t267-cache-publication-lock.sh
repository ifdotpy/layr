#!/bin/bash
# ci: macos macos-plain overlayfs
# An overlay lock cannot make a prepared cache save block under its parent project lock.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo initial > "$T/src/file"
# A regenerable plain folder becomes a cache only when the ignore rules ignore it.
echo target/ > "$T/src/.gitignore"
l init cache-publish --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/cache-publish
cd "$P/lines/main"
l protect main --remove >/dev/null
l line new -q other || exit 1
l migrate >/dev/null || exit 1
l config regenerable '["target"]' >/dev/null
mkdir target
echo cache > target/output
echo changed > file
python3 - "$T" "$LAYR" "$P" "$TG" <<'PY'
import fcntl
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
import traceback

root, binary, project, overlay = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]), sys.argv[4]
main, other = project / 'lines/main', project / 'lines/other'
overlays = project / 'meta/overlays'
barrier, final_barrier = root / 'publish', root / 'final-cache'
def diagnose_failure(kind, error, tb):
    # Every uncaught assertion/timeout gets both process logs and bounded directory listings,
    # including failures in the victim setup before the deleter has started.
    print(f't267 failure: expected overlay={overlay}, platform={os.environ.get("LAYR_TEST_PLATFORM")}', file=sys.stderr, flush=True)
    for name in ('save.log', 'delete.log', 'commands.log'):
        path = root / name
        print(f'--- {path} ---', file=sys.stderr, flush=True)
        print(path.read_text(errors='replace') if path.exists() else '(not created)', file=sys.stderr, flush=True)
    paths = [overlays]
    if overlays.is_dir():
        paths += sorted(p for p in overlays.iterdir() if p.is_dir() and not p.is_symlink())
        paths += sorted(p for parent in paths[1:] for p in parent.iterdir() if p.is_dir() and not p.is_symlink())
    for path in [*paths, barrier, final_barrier]:
        print(f'--- ls -la {path} ---', file=sys.stderr, flush=True)
        subprocess.run(['ls', '-la', str(path)], stdout=sys.stderr, stderr=sys.stderr, check=False)
    traceback.print_exception(kind, error, tb)
sys.excepthook = diagnose_failure
def run(*args, cwd=main, timeout=20):
    with (root / 'commands.log').open('a') as log:
        print(f'{args!r} cwd={cwd}', file=log, flush=True)
        result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=timeout)
        print(f'exit={result.returncode}\n{result.stdout}{result.stderr}', file=log, flush=True)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result
def states():
    with sqlite3.connect(project / 'meta/db/layr.sqlite') as db:
        return {row[0] for row in db.execute('SELECT id FROM states')}
before = states()
barrier.mkdir()
with (root / 'save.log').open('w') as log:
    saver = subprocess.Popen([binary, 'save', '-q'], cwd=main,
                             env=dict(os.environ, LAYR_TRACE="1", LAYR_TEST_SAVE_PUBLISH_BARRIER=str(barrier)), stdout=log, stderr=log)
try:
    deadline = time.monotonic() + 20
    while not (barrier / 'ready').exists():
        assert saver.poll() is None, (root / 'save.log').read_text()
        assert time.monotonic() < deadline
        time.sleep(.01)
    overlay_project = overlays / overlay
    overlay_lock = overlay_project / 'meta/lock'
    assert overlay_lock.parent.is_dir(), 'ignored regenerable target did not create the overlay project'
    assert any((p / 'tree').is_dir() for p in (overlay_project / 'meta/work').iterdir()), 'save has no prepared cache part'
    # Project::lock_wait creates meta/lock lazily. Preparation has not locked this
    # new overlay yet, so the test must create the same file before taking its flock.
    with overlay_lock.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        (barrier / 'release').touch()
        assert saver.wait(timeout=5) != 0, 'three stale publication attempts must stop'
        assert 'line changed during save publication' in (root / 'save.log').read_text(), (root / 'save.log').read_text()
        with (project / 'meta/lock').open('a') as parent:
            fcntl.flock(parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert states() == before, 'a busy overlay publishes no partial parent state'
        run('tag', 'overlay-busy', 'head', cwd=other, timeout=3)
    run('save', '-q')
    assert len(states() - before) == 1
finally:
    if saver.poll() is None:
        saver.kill()
        saver.wait()
# Once the victim has a cache part, final-state publication needs no new cache capture.
# Its later orphan cleanup must also defer when that overlay is busy.
run('line', 'new', '-q', 'victim', 'main')
victim = project / 'lines/victim'
run('save', '-q', cwd=victim)
# Confirm the victim already has its own saved cache part: final deletion should only
# encounter this busy overlay during the separately validated orphan-cleanup phase.
with sqlite3.connect(overlay_project / 'meta/db/layr.sqlite') as db:
    cached = db.execute("SELECT json_extract(states.rec, '$.cache_of') FROM lines JOIN states ON states.id=lines.head WHERE lines.name='victim'").fetchone()
assert cached is not None and cached[0] is not None, 'victim has no saved cache part'
(victim / 'file').write_text('final bytes')
final_barrier.mkdir()
with (root / 'delete.log').open('w') as log:
    deleter = subprocess.Popen([binary, 'line', 'delete', '-D', '--discard', 'victim'], cwd=main,
                              env=dict(os.environ, LAYR_TRACE="1", LAYR_TEST_SAVE_CACHE_BARRIER=str(final_barrier)), stdout=log, stderr=log)
try:
    deadline = time.monotonic() + 20
    while not (final_barrier / 'ready').exists():
        assert deleter.poll() is None, (root / 'delete.log').read_text()
        assert time.monotonic() < deadline
        time.sleep(.01)
    with overlay_lock.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        (final_barrier / 'release').touch()
        assert deleter.wait(timeout=5) == 0, (root / 'delete.log').read_text()
        assert not victim.exists()
        run('tag', 'cleanup-busy', 'head', cwd=other, timeout=3)
finally:
    if deleter.poll() is None:
        deleter.kill()
        deleter.wait()
run('gc', '--repair')
print('cache publication and final deletion never wait under the parent lock', flush=True)
PY
ok "cache publication never waits under the parent lock" test "$?" = 0
finish
