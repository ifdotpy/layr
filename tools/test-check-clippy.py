#!/usr/bin/env python3
"""Exercise manifest enforcement without compiling the workspace."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

script = Path(__file__).with_name('check-clippy.py').resolve()
with tempfile.TemporaryDirectory() as temp:
    root = Path(temp)
    (root / 'tools').mkdir()
    (root / 'tools/clippy-baseline.json').write_text('{}')
    manifest = root / 'Cargo.toml'
    cargo = root / 'cargo'
    metadata = {'packages': [{'id': 'test', 'name': 'test', 'manifest_path': str(manifest)}]}
    cargo.write_text('#!/usr/bin/env python3\nimport sys\nif "metadata" in sys.argv:\n print(' + repr(json.dumps(metadata)) + ')\n')
    cargo.chmod(0o755)
    env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'])
    for content, expected in [('[package]\nname = "test"\n', 1), ('[lints]\nworkspace = true\n', 0)]:
        manifest.write_text('[workspace.lints.rust]\n[workspace.lints.clippy]\n' + content)
        result = subprocess.run([sys.executable, str(script)], cwd=root, env=env, capture_output=True, text=True)
        if result.returncode != expected or (expected and 'missing [lints]' not in result.stderr):
            raise SystemExit(f'unexpected result: {result.returncode}, {result.stdout}, {result.stderr}')
print('Clippy inheritance regression OK')
