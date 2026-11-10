#!/usr/bin/env python3
"""Check that the integration test runner respects the process result."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class RunnerResultTest(unittest.TestCase):
    def run_case(self, script):
        with tempfile.TemporaryDirectory(prefix="layr-runner-test-") as tmp:
            root = Path(tmp)
            tests = root / "tests"
            tests.mkdir()
            shutil.copy(Path(__file__).with_name("run.sh"), tests / "run.sh")
            (tests / "t-result.sh").write_text("#!/bin/bash\n" + script)
            env = dict(os.environ, TMPDIR=tmp, LAYR_ROOT=str(root / "store"))
            return subprocess.run(
                ["bash", "tests/run.sh", "t-result"],
                cwd=root,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_success_needs_zero_exit_and_passing_summary(self):
        result = self.run_case('echo "1 passed, 0 failed"\n')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_nonzero_exit_overrides_passing_summary(self):
        result = self.run_case('echo "1 passed, 0 failed"\nexit 17\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAIL t-result: exit 17", result.stdout)

    def test_zero_exit_without_passing_summary_fails(self):
        result = self.run_case('echo "incomplete"\n')
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
