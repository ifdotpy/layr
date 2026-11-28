#!/usr/bin/env python3
"""Check the integration test runner schedules longer tests first."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class OrderTest(unittest.TestCase):
    def test_parallel_tests_start_longest_first(self):
        bash = "/opt/homebrew/bin/bash" if Path("/opt/homebrew/bin/bash").exists() else "bash"
        with tempfile.TemporaryDirectory(prefix="layr-runner-test-") as tmp:
            root = Path(tmp)
            tests = root / "tests"
            tests.mkdir()
            shutil.copy(Path(__file__).with_name("run.sh"), tests / "run.sh")
            for name in ("ta", "tb", "tc"):
                (tests / f"{name}.sh").write_text(f'#!/bin/bash\necho "{name}: 1 passed, 0 failed"\n')
            (tests / "durations.txt").write_text("# seconds name\n300 tc\n50 ta\n")
            env = dict(os.environ, TMPDIR=tmp, LAYR_ROOT=str(root / "store"), LAYR_TEST_JOBS="2")
            result = subprocess.run([bash, "-c", 'v=${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}; [[ $v > 4.2 ]] || exit 77; '
                                     'exec "$BASH" tests/run.sh ta tb tc'], cwd=root, env=env, text=True,
                                    capture_output=True, check=False)
            if result.returncode == 77:
                self.skipTest("parallel tests need bash 4.3")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            # The runner reports in its start order. Two jobs start at once, so the order of their own
            # writes is random; the reports are not. tb is not listed: it counts as 10 s, after ta (50 s).
            order = [line.split(":")[0] for line in result.stdout.splitlines() if line.endswith(", 0 failed")]
            self.assertEqual(order, ["tc", "ta", "tb"], result.stdout)

if __name__ == "__main__":
    unittest.main()
