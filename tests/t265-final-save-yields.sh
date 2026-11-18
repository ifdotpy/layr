#!/bin/bash
# ci: macos macos-plain overlayfs
# Final deletion retries a late writer outside the project lock.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo initial > "$T/src/file"
l init final --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/final
cd "$P/lines/main"
l line new -q topic || exit 1
python3 - "$T" "$LAYR" "$P" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import time

root, binary, project = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
main, topic = project / 'lines/main', project / 'lines/topic'
(topic / 'file').write_text('early')
barrier = root / 'comparison'
barrier.mkdir()
with (root / 'delete.log').open('w') as log:
    deleter = subprocess.Popen([binary, 'line', 'delete', '-D', '--discard', 'topic'], cwd=main,
                              env=dict(os.environ, LAYR_TEST_SAVE_COMPARISON_BARRIER=str(barrier)), stdout=log, stderr=log)
try:
    deadline = time.monotonic() + 20
    while not (barrier / 'ready').exists():
        assert deleter.poll() is None, (root / 'delete.log').read_text()
        assert time.monotonic() < deadline
        time.sleep(.01)
    other = subprocess.run([binary, 'tag', 'during-final', 'head'], cwd=main, capture_output=True, timeout=3)
    assert other.returncode == 0, other.stderr
    before = (topic / 'file').stat()
    (topic / 'file').write_text('later')
    os.utime(topic / 'file', ns=(before.st_atime_ns, before.st_mtime_ns))
    (topic / 'late').write_text('late new file')
    (barrier / 'release').touch()
    assert deleter.wait(timeout=20) == 0, (root / 'delete.log').read_text()
    result = subprocess.run([binary, 'undo'], cwd=main, capture_output=True, timeout=20)
    assert result.returncode == 0, result.stderr
    assert (topic / 'file').read_text() == 'later'
    assert (topic / 'late').read_text() == 'late new file'
finally:
    if deleter.poll() is None:
        deleter.kill()
        deleter.wait()
print('late write with restored mtime is preserved', flush=True)
PY
ok "final deletion yields and preserves late writes" test "$?" = 0
finish
