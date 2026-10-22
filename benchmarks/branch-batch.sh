#!/bin/bash
# Run as root on Linux with the binary in LAYR. Print raw trials as JSON lines.
set -euo pipefail
. "$(dirname "$0")/../tests/lib.sh"
mkdir -p "$T/src"
for n in $(seq 1 100); do echo "source $n" > "$T/src/file-$n"; done
l init batch-bench --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/batch-bench/lines/main"
python3 - "$LAYR" <<'PY'
import json, subprocess, sys, time
binary = sys.argv[1]
for count in [10, 100]:
    for trial in range(3):
        # Alternate which mode runs first to reduce order effects.
        for mode in (["batch", "single"] if trial % 2 == 0 else ["single", "batch"]):
            prefix = f"{mode}-{count}-{trial}"
            start = time.perf_counter()
            if mode == "batch":
                subprocess.run([binary, "branch", prefix, "HEAD", "--count", str(count), "-q"], check=True)
            else:
                for index in range(1, count + 1):
                    subprocess.run([binary, "branch", f"{prefix}-{index}", "HEAD", "-q"], check=True)
            print(json.dumps(dict(count=count, trial=trial + 1, mode=mode, milliseconds=round((time.perf_counter()-start)*1000, 3))), flush=True)
PY
