#!/bin/bash
# ci: macos macos-plain overlayfs
# A busy publication leaves an unchanged periodic check due for the next tick.
. "$(dirname "$0")/lib.sh"
export LAYR_TEST_PERIODIC=1
mkdir -p "$T/src"
echo initial > "$T/src/file"
l init periodic-publish --from "$T/src" -q || exit 1
P=$LAYR_ROOT/projects/periodic-publish
cd "$P/lines/main"
l config snapshot.interval 0 >/dev/null
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
meta = project / 'meta'
with sqlite3.connect(meta / 'db/layr.sqlite') as db:
    line = db.execute("SELECT id FROM lines WHERE name='main'").fetchone()[0]
barrier = root / 'cache'
barrier.mkdir()
with (root / 'periodic.log').open('w') as log:
    periodic = subprocess.Popen([binary, '__test-periodic', os.environ['LAYR_ROOT']],
                                env=dict(os.environ, LAYR_TEST_SAVE_CACHE_BARRIER=str(barrier)), stdout=log, stderr=log)
try:
    deadline = time.monotonic() + 20
    while not (barrier / 'ready').exists():
        assert periodic.poll() is None, (root / 'periodic.log').read_text()
        assert time.monotonic() < deadline
        time.sleep(.01)
    with (meta / 'lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        (barrier / 'release').touch()
        assert periodic.wait(timeout=5) == 0
    checks = meta / 'periodic-checks.json'
    assert not checks.exists() or line not in json.loads(checks.read_text()), 'skipped publication advanced timestamp'
    result = subprocess.run([binary, '__test-periodic', os.environ['LAYR_ROOT']], capture_output=True, timeout=20)
    assert result.returncode == 0, result.stderr
    assert line in json.loads(checks.read_text()), 'next tick did not retry the unchanged line'
finally:
    if periodic.poll() is None:
        periodic.kill()
        periodic.wait()
print('a skipped publication remains due', flush=True)
PY
ok "periodic publication skips without advancing timestamps" test "$?" = 0
finish
