#!/usr/bin/env python3
"""Exercise the real shell runner with private, non-root test fixtures."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

RUNNER = Path(__file__).resolve().parents[2] / "tests/run.sh"
BASH = os.environ.get("LAYR_TEST_BASH", shutil.which("bash"))
VERSION = tuple(map(int, subprocess.check_output(
    [BASH, "-c", 'echo "${BASH_VERSINFO[0]} ${BASH_VERSINFO[1]}"'], text=True).split()))
PARALLEL = VERSION >= (4, 3)
JOB_COUNTS = ("1", "2") if PARALLEL else ("1",)


class RunnerTests(unittest.TestCase):
    def run_fixture(self, scripts, jobs="1", times=False):
        with tempfile.TemporaryDirectory(prefix="layr-runner-test-") as tmp:
            root = Path(tmp)
            (root / "tests").mkdir()
            shutil.copyfile(RUNNER, root / "tests/run.sh")
            for name, body in scripts.items():
                (root / "tests" / (name + ".sh")).write_text(body)
            env = dict(os.environ, TMPDIR=tmp, LAYR_ROOT=str(root / "store"),
                       LAYR_TEST_JOBS=jobs, LAYR_TEST_TIMEOUT="5")
            env.pop("LAYR_TEST_TIMES", None)
            if times:
                env["LAYR_TEST_TIMES"] = "1"
            return subprocess.run([BASH, "tests/run.sh", *scripts], cwd=root,
                                  env=env, text=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, timeout=30)

    def test_nonzero_exit_after_success_summary(self):
        for jobs in JOB_COUNTS:
            with self.subTest(jobs=jobs):
                result = self.run_fixture({"t01": "echo '1 passed, 0 failed'\nexit 9\n"}, jobs)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("exit 9", result.stdout)

    def test_summary_must_end_with_zero_failures(self):
        result = self.run_fixture({"t01": "echo '1 passed, 0 failed but incomplete'\n"})
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_success_with_timings(self):
        for jobs in JOB_COUNTS:
            with self.subTest(jobs=jobs):
                result = self.run_fixture({"t01": "echo '1 passed, 0 failed'\n"}, jobs, True)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertRegex(result.stdout, r"1 passed, 0 failed \(\d+ s\)")

    def test_missing_summary_fails(self):
        result = self.run_fixture({"t01": "echo incomplete\n"}, JOB_COUNTS[-1])
        self.assertNotEqual(result.returncode, 0, result.stdout)

    @unittest.skipUnless(PARALLEL, "parallel tests need Bash 4.3")
    def test_parallel_output_order_and_serial_isolation(self):
        result = self.run_fixture({
            "t01": "touch active\nsleep 0.2\nrm active\necho 'first: 1 passed, 0 failed'\n",
            "t02": "echo 'second: 1 passed, 0 failed'\n",
            "t03": "# serial\ntest ! -e active || exit 8\necho 'serial: 1 passed, 0 failed'\n",
        }, "2")
        self.assertEqual(result.returncode, 0, result.stdout)
        lines = result.stdout.splitlines()
        self.assertEqual([line.split(':')[0] for line in lines], ["first", "second", "serial"])

    def test_invalid_job_counts_fail(self):
        for jobs in ("0", "-1", "abc", "1+1"):
            with self.subTest(jobs=jobs):
                result = self.run_fixture({"t01": "echo '1 passed, 0 failed'\n"}, jobs)
                self.assertNotEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main()
