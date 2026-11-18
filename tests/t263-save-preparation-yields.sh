#!/bin/bash
# ci: macos macos-plain overlayfs
# Every expensive phase runs with only the saved line locked.
. "$(dirname "$0")/lib.sh"
export LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK=1
mkdir -p "$T/src"
echo initial > "$T/src/file"
echo target/ > "$T/src/.gitignore"
l init prep --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/prep
cd "$P/lines/main"
l protect main --remove >/dev/null
l line new -q other || exit 1
python3 - "$T" "$LAYR" "$P" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main, other = project / 'lines/main', project / 'lines/other'
def run(*args, cwd=main, timeout=15):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=timeout)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result
# Keep migration before nested cache creation until https://github.com/ifdotpy/layr/issues/449 is fixed.
for phase in ('snapshot', 'comparison', 'objects', 'publish', 'cache'):
    barrier = root / phase
    barrier.mkdir()
    if phase == 'objects':
        run('migrate')
    (main / 'file').write_text(phase)
    # The cache phase includes a real first cache state, not only a no-op barrier.
    if phase == 'cache':
        run('config', 'regenerable', '["target"]')
        assert (main / 'target').is_dir(), 'config must initialize the cold cache'
        (main / 'target/output').write_text('cached bytes')
    env = dict(os.environ, **{f'LAYR_TEST_SAVE_{phase.upper()}_BARRIER': str(barrier)})
    with (root / f'{phase}.log').open('w') as log:
        saver = subprocess.Popen([binary, 'save', '-q'], cwd=main, env=env, stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 20
        while not (barrier / 'ready').exists():
            assert saver.poll() is None, (root / f'{phase}.log').read_text()
            assert time.monotonic() < deadline, f'{phase} did not reach its barrier'
            time.sleep(.01)
        run('tag', f'during-{phase}', 'head', cwd=other, timeout=3)
        assert saver.poll() is None, 'the unrelated command finished while capture was blocked'
        if phase == 'publish':
            before = (main / 'file').stat()
            (main / 'file').write_text('Publish')
            os.utime(main / 'file', ns=(before.st_atime_ns, before.st_mtime_ns))
            (main / 'late').write_text('edited while publication waited')
        (barrier / 'release').touch()
        assert saver.wait(timeout=20) == 0, (root / f'{phase}.log').read_text()
        assert 'project lock acquired during save preparation' not in (root / f'{phase}.log').read_text()
        if phase == 'publish':
            assert run('show', 'head@{0}:file').stdout == 'Publish'
            assert 'edited while publication waited' in run('show', 'head@{0}:late').stdout
    finally:
        if saver.poll() is None:
            saver.kill()
            saver.wait()
    print(f'{phase}: another line acquired the project lock', flush=True)
PY
ok "each preparation phase yields the project lock" test "$?" = 0
finish
