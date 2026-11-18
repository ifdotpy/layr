#!/bin/bash
# ci: macos overlayfs
# Undo keeps the last running copy after three attempts when a writer never settles.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo original > "$T/src/file"
l init undo-unsettled --from "$T/src" -q || exit 1
if [ "$(uname -s)" != Darwin ] && [ ! -d "$LAYR_ROOT/.layr-overlayfs" ]; then
    echo 'SKIP: native btrfs snapshots cannot produce an unsettled copy'
    finish
    exit "$?"
fi
P=$LAYR_ROOT/projects/undo-unsettled
cd "$P/lines/main" || exit 1
l line new -q victim || exit 1
python3 - "$T" "$LAYR" "$P" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main = project / 'lines/main'
work = project / 'lines/victim'
def run(*args, cwd=main):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, f'{args}: {result.stdout}\n{result.stderr}'
    return result.stdout
created = json.loads(run('op', 'log', '--all', '-n', '1', '--json'))['id']
(work / 'sentinel').write_text('recover this file\n')
(work / 'hot').write_text('initial\n')
gates, log_path = root / 'copy', root / 'undo.log'
env = dict(os.environ, LAYR_TEST_SAVE_COPY_BARRIER=str(gates),
           LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK='1')
with log_path.open('w') as log:
    undo = subprocess.Popen([binary, 'undo', created], cwd=main, env=env, stdout=log, stderr=log)
checks = 0
try:
    deadline = time.monotonic() + 90
    while undo.poll() is None:
        for gate in gates.glob('*'):
            if (gate / 'ready').exists() and not (gate / 'release').exists():
                # Act as a writer after every copy, before its validation scan. This
                # keeps every capture unsettled without relying on scheduling luck.
                with (work / 'hot').open('a') as hot:
                    hot.write(f'write {checks}\n')
                checks += 1
                (gate / 'release').touch()
        assert time.monotonic() < deadline, f'undo timed out: {log_path.read_text()}'
        time.sleep(.01)
    output = log_path.read_text()
    assert undo.returncode == 0, f'undo failed ({undo.returncode}): {output}'
    assert checks >= 9, f'expected three unsettled captures, got only {checks} copy rounds: {output}'
    assert output.count('kept changing while it was copied') == 1, output
    assert not work.exists(), 'undo must remove the created line'
finally:
    if undo.poll() is None:
        undo.kill()
        undo.wait()
undone = json.loads(run('op', 'log', '--all', '-n', '1', '--json'))['id']
run('undo', undone)
assert (work / 'sentinel').read_text() == 'recover this file\n', 'undo of undo lost the captured file'
assert 'write ' in (work / 'hot').read_text(), 'undo of undo lost the running copy'
history = run('log', '--auto', cwd=work)
assert 'taken while processes ran' in history, f'the recovery capture must be marked running: {history}'
print(f'undo kept a running capture after {checks} copy validation rounds and warned once')
PY
ok "undo retries unsettled copies and keeps the last recovery capture" test "$?" = 0
finish
