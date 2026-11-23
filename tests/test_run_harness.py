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


class OrderTest(unittest.TestCase):
    def test_parallel_tests_start_longest_first(self):
        bash = "/opt/homebrew/bin/bash" if Path("/opt/homebrew/bin/bash").exists() else "bash"
        with tempfile.TemporaryDirectory(prefix="layr-runner-test-") as tmp:
            root = Path(tmp)
            tests = root / "tests"
            tests.mkdir()
            shutil.copy(Path(__file__).with_name("run.sh"), tests / "run.sh")
            started = root / "started"
            for name in ("ta", "tb", "tc"):
                (tests / f"{name}.sh").write_text(f'#!/bin/bash\necho {name} >> {started}\necho "1 passed, 0 failed"\n')
            (tests / "durations.txt").write_text("# seconds name\n300 tc\n50 ta\n")
            env = dict(os.environ, TMPDIR=tmp, LAYR_ROOT=str(root / "store"), LAYR_TEST_JOBS="2")
            result = subprocess.run([bash, "-c", 'v=${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}; [[ $v > 4.2 ]] || exit 77; '
                                     'exec "$BASH" tests/run.sh ta tb tc'], cwd=root, env=env, text=True,
                                    capture_output=True, check=False)
            if result.returncode == 77:
                self.skipTest("parallel tests need bash 4.3")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            # tb is not listed: it counts as 10 s, after ta (50 s).
            self.assertEqual(started.read_text().split()[0], "tc")
            self.assertEqual(result.stdout.split().count("passed,"), 3)

if __name__ == "__main__":
    unittest.main()
