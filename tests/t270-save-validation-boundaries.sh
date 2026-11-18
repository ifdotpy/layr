#!/bin/bash
# ci: macos macos-plain overlayfs
# Foreground fences ignore saved cache churn and retry disappearing source entries.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo initial > "$T/src/file"
echo target/ > "$T/src/.gitignore"
l init validation --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/validation
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null
python3 - "$T" "$LAYR" "$P" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main = project / 'lines/main'
def run(*args, cwd=main, env=None, success=True):
    result = subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=20)
    assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
    return result
def ready(process, gate, log):
    deadline = time.monotonic() + 20
    while not (gate / 'ready').exists():
        assert process.poll() is None, log.read_text()
        assert time.monotonic() < deadline, (gate, log.read_text())
        time.sleep(.01)

run('config', 'regenerable', '["target"]')
assert (main / 'target').is_dir(), 'config must initialize the cold cache'
(main / 'target/output').write_text('already saved cache')
run('save', '--force', '-q')
run('line', 'new', '-q', 'other')
stop, started = threading.Event(), threading.Event()
errors, writes = [], [0]
def compile_cache():
    try:
        while not stop.is_set():
            output = main / 'target/churn'
            output.mkdir()
            (output / 'object').write_bytes(b'compiler output')
            (output / 'object').unlink()
            output.rmdir()
            writes[0] += 1
            started.set()
    except BaseException as error:
        errors.append(error)
writer = threading.Thread(target=compile_cache)
writer.start()
try:
    assert started.wait(timeout=5)
    retry = root / 'unexpected-cache-retry'
    retry.mkdir()
    env = dict(os.environ, LAYR_TEST_SAVE_RETRY_BARRIER=str(retry), LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1')
    for args in [('save', '-q'), ('save', '-q'), ('restore', 'file'),
                 ('reset', '--soft', '-q', 'head'), ('undo',), ('merge', '-q', 'other')]:
        run(*args, env=env)
    assert not (retry / 'ready').exists(), 'cache churn invalidated a source preparation'
    assert (main / 'file').read_text() == 'initial\n'
    assert writes[0] > 20, writes[0]
finally:
    stop.set()
    writer.join(timeout=5)
assert not writer.is_alive() and not errors, errors
print('saved cache churn does not enter or invalidate foreground source fences', flush=True)

# Ignoring target must not ignore a source sibling edited after content capture.
scan, retry = root / 'sibling-scan', root / 'sibling-retry'
scan.mkdir()
retry.mkdir()
log_path = root / 'sibling-edit.log'
env = dict(os.environ, LAYR_TEST_SAVE_SCAN_ENTRY='target',
           LAYR_TEST_SAVE_SCAN_AFTER_BARRIER=str(scan), LAYR_TEST_SAVE_RETRY_BARRIER=str(retry),
           LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1')
with log_path.open('w') as log:
    saver = subprocess.Popen([binary, 'save', '--force', '-q'], cwd=main, env=env, stdout=log, stderr=log)
try:
    ready(saver, scan, log_path)
    (main / 'file').write_text('late sibling edit\n')
    (scan / 'release').touch()
    ready(saver, retry, log_path)
    run('tag', 'during-sibling-retry', 'head')
    (retry / 'release').touch()
    assert saver.wait(timeout=20) == 0, log_path.read_text()
    assert run('show', 'head@{0}:file').stdout == 'late sibling edit\n'
finally:
    if saver.poll() is None:
        saver.kill()
        saver.wait()
print('a late source sibling edit invalidates preparation outside the project lock', flush=True)

# Stop at an entry after enumeration, remove it, and require a retry outside the
# project lock. Cover both preparation scans and both foreground retry callers.
for final in (False, True):
    for phase in ('before', 'after'):
        name = f'race-{final}-{phase}'
        run('line', 'new', '-q', name)
        work = project / 'lines' / name
        (work / 'vanish').write_text('removed between enumeration and stat')
        scan, retry = root / f'{name}-scan', root / f'{name}-retry'
        scan.mkdir()
        retry.mkdir()
        log_path = root / f'{name}.log'
        env = dict(os.environ, LAYR_TEST_SAVE_SCAN_ENTRY='vanish',
                   LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1',
                   **{f'LAYR_TEST_SAVE_SCAN_{phase.upper()}_BARRIER': str(scan)})
        if not final:
            env['LAYR_TEST_SAVE_RETRY_BARRIER'] = str(retry)
        args = ['line', 'delete', '-D', '--discard', name] if final else ['save', '--force', '-q']
        with log_path.open('w') as log:
            saver = subprocess.Popen([binary, *args], cwd=main if final else work,
                                     env=env, stdout=log, stderr=log)
        try:
            ready(saver, scan, log_path)
            (work / 'vanish').unlink()
            (scan / 'release').touch()
            if not final:
                ready(saver, retry, log_path)
                run('tag', name, 'head')
                (retry / 'release').touch()
            assert saver.wait(timeout=20) == 0, log_path.read_text()
            if final:
                run('undo')
                assert work.is_dir(), 'undo must restore the deleted line without pre-saving its tombstone'
            assert not (work / 'vanish').exists()
            if not final:
                run('show', 'head@{0}:vanish', cwd=work, success=False)
        finally:
            if saver.poll() is None:
                saver.kill()
                saver.wait()
        print(f'{name}: disappearance retried successfully', flush=True)
PY
ok "cache churn and source disappearance respect save boundaries" test "$?" = 0
finish
