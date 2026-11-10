#!/usr/bin/env python3
"""The macOS check seed selects the Rust version of the checked state."""
import os
import pathlib
import platform
import subprocess
import tempfile
import unittest


@unittest.skipUnless(platform.system() == 'Darwin', 'requires macOS')
class ToolchainTests(unittest.TestCase):
    def test_state_version_and_missing_version(self):
        script = pathlib.Path(__file__).resolve().parents[1] / 'tools/checks/environment.sh'
        with tempfile.TemporaryDirectory(prefix='layr-macos-select-') as directory:
            root = pathlib.Path(directory)
            seed, work = root / 'seed', root / 'work'
            work.mkdir()
            (seed / 'cargo/registry').mkdir(parents=True)
            (seed / 'cargo/bin').mkdir()
            (seed / 'rustup/toolchains').mkdir(parents=True)
            (seed / 'toolchain').write_text('1.98.0-aarch64-apple-darwin\n')
            (seed / 'clang').write_text('/usr/bin/cc\n')
            (seed / 'sdk').write_text('/tmp\n')
            (seed / 'git').write_text('/usr/bin/git\n')
            for version in ('1.98.0', '1.99.1'):
                rustc = seed / f'rustup/toolchains/{version}-aarch64-apple-darwin/bin/rustc'
                rustc.parent.mkdir(parents=True)
                rustc.touch()
                rustc.chmod(0o755)
            for version in ('1.98.0', '1.99.1', '1.97.0'):
                home = root / f'home-{version}'
                home.mkdir()
                (work / 'rust-toolchain.toml').write_text(f'[toolchain]\nchannel = "{version}"\n')
                result = subprocess.run(['bash', '-c', 'source "$1"; printf "%s" "$PATH"', 'test', str(script)],
                                        cwd=work, env={**os.environ, 'HOME': str(home), 'LAYR_CHECK_SEED': str(seed)},
                                        text=True, capture_output=True)
                if version == '1.97.0':
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('Prepare Rust', result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(f'{version}-aarch64-apple-darwin/bin', result.stdout)
                    self.assertEqual(result.stderr, '')


if __name__ == '__main__':
    unittest.main()
