#!/bin/bash
# ci: macos macos-plain overlayfs
# Scan races retry; only the last unsettled capture can waive the content fence.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo initial > "$T/src/file"
l init unsettled-races --from "$T/src" -q || exit 1
COPY_BACKEND=0
if [ "$(uname -s)" = Darwin ] || [ -d "$LAYR_ROOT/.layr-overlayfs" ]; then
    COPY_BACKEND=1
fi
P=$LAYR_ROOT/projects/unsettled-races
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null
python3 - "$T" "$LAYR" "$P" "$COPY_BACKEND" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main = project / 'lines/main'
def run(*args, cwd=main):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, f'{args} failed ({result.returncode}): {result.stdout}\n{result.stderr}'
    return result.stdout
def wait_for(process, predicate, description, log, advance):
    deadline = time.monotonic() + 20
    while not predicate():
        advance()
        if process.poll() is not None:
            assert predicate(), f'{description}: saver exited ({process.returncode}): {log.read_text()}'
            return
        assert time.monotonic() < deadline, f'{description}: timed out: {log.read_text()}'
        time.sleep(.01)
def ready(process, gate, log, advance):
    wait_for(process, lambda: (gate / 'ready').exists(), f'waiting for {gate.name}', log, advance)

# btrfs snapshots cannot be unsettled, but must still reject every scan race.
# Copy backends change the source at every copy check to force real unsettled captures.
for unsettled in ([False, True] if sys.argv[4] == '1' else [False]):
    for phase in ('before', 'after'):
        for race in ('disappearance', 'type-change'):
            name = f'{unsettled}-{phase}-{race}'
            run('line', 'new', '-q', name)
            work = project / 'lines' / name
            before = run('log', '--auto', cwd=work)
            (work / 'hot.txt').write_text('initial copy input\n')
            victim = work / 'victim'
            victim.write_text('copied before the scan race')
            saver = None
            log_path = root / f'{name}.log'
            scan, retry = root / f'{name}-scan', root / f'{name}-retry'
            scan.mkdir()
            retry.mkdir()
            copy = root / f'{name}-copy'
            def advance():
                for check in copy.glob('*'):
                    if (check / 'ready').exists() and not (check / 'release').exists():
                        # Mutate after this round copied and before its validation scan.
                        # A busy writer alone can be descheduled for an entire short copy.
                        with (work / 'hot.txt').open('a') as hot:
                            hot.write('changed\n')
                        (check / 'release').touch()
            env = dict(os.environ, LAYR_TEST_SAVE_SCAN_ENTRY='victim',
                       LAYR_TEST_SAVE_RETRY_BARRIER=str(retry),
                       LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1',
                       **{f'LAYR_TEST_SAVE_SCAN_{phase.upper()}_BARRIER': str(scan)})
            if unsettled:
                env['LAYR_TEST_SAVE_COPY_BARRIER'] = str(copy)
            try:
                with log_path.open('w') as log:
                    saver = subprocess.Popen([binary, 'save', '--force', '-q'], cwd=work, env=env, stdout=log, stderr=log)
                for attempt in range(3):
                    ready(saver, scan, log_path, advance)
                    victim.unlink()
                    if race == 'type-change':
                        victim.mkdir()
                    (scan / 'release').touch()
                    if unsettled and attempt == 2:
                        # The third running capture is kept even when its content fence races.
                        break
                    ready(saver, retry, log_path, advance)
                    if unsettled:
                        warnings = log_path.read_text().count('kept changing while it was copied')
                        assert warnings == 1, f'{name}, attempt {attempt + 1}: expected one unsettled warning, got {warnings}: {log_path.read_text()}'
                    if attempt < 2:
                        if victim.is_dir():
                            victim.rmdir()
                        victim.write_text('copied before the scan race')
                        (scan / 'ready').unlink()
                        (scan / 'release').unlink()
                    (retry / 'ready').unlink()
                    (retry / 'release').touch()
                    if attempt < 2:
                        ready(saver, scan, log_path, advance)
                        (retry / 'release').unlink()
                # Keep servicing copy gates after the final before-scan race, too.
                wait_for(saver, lambda: saver.poll() is not None, f'{name}: waiting for save completion', log_path, advance)
                expected = 0 if unsettled else 128
                assert saver.returncode == expected, f'{name}: expected exit {expected}, got {saver.returncode}: {log_path.read_text()}'
                history = run('log', '--auto', cwd=work)
                if unsettled:
                    assert 'taken while processes ran' in history, f'{name}: published capture is not marked running: {history}'
                    assert history != before, f'{name}: the running capture must be published'
                    warnings = log_path.read_text().count('kept changing while it was copied')
                    assert warnings == 1, f'{name}: expected one unsettled warning, got {warnings}: {log_path.read_text()}'
                    if phase == 'after':
                        captured = run('show', 'head@{0}:victim', cwd=work)
                        assert captured == 'copied before the scan race', f'{name}: last capture lost victim contents: {captured!r}'
                else:
                    assert 'line changed during save publication; retry the command' in log_path.read_text(), f'{name}: missing stale-publication error: {log_path.read_text()}'
                    assert history == before, f'{name}: a settled capture with a stale scan must not be published: {history}'
            finally:
                if saver is not None and saver.poll() is None:
                    saver.kill()
                    saver.wait()
            print(f'{name}: scan races {"keep the last running capture" if unsettled else "reject settled captures"}', flush=True)
PY
ok "scan races retry and preserve the last unsettled capture" test "$?" = 0
finish
