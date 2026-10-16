#!/usr/bin/env python3
"""Compare full-tree and delta snapshots in an isolated OverlayFS test store."""
import argparse
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

p = argparse.ArgumentParser()
p.add_argument('--before', required=True)
p.add_argument('--after', required=True)
p.add_argument('--root', required=True)
p.add_argument('--files', type=int, default=10000)
p.add_argument('--repeats', type=int, default=3)
a = p.parse_args()
root = Path(a.root).resolve()
root.mkdir(exist_ok=False)
report = {}
for label, binary in [('before', a.before), ('after', a.after)]:
    case = root / label
    subprocess.run([sys.executable, str(Path(__file__).with_name('backends.py')),
                    '--binary', binary, '--root', str(case), '--files', str(a.files),
                    '--repeats', str(a.repeats)], check=True)
    result = json.loads((case / 'results.json').read_text())
    seen = set()
    allocated = logical = files = 0
    for folder, _, names in os.walk(case / 'store/.layr-overlayfs/objects'):
        for name in names:
            m = (Path(folder) / name).lstat()
            identity = (m.st_dev, m.st_ino)
            if identity in seen or not stat.S_ISREG(m.st_mode):
                continue
            seen.add(identity)
            files += 1
            logical += m.st_size
            allocated += m.st_blocks * 512
    # On XFS, st_blocks can count a reflink extent more than once.
    result['private_regular_files'] = files
    result['private_logical_bytes'] = logical
    result['private_allocated_bytes_without_reflink_dedup'] = allocated
    report[label] = result
(root / 'comparison.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({k: {n: v[n] for n in v if n.startswith('private_')} for k, v in report.items()}))
