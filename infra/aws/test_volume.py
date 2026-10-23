"""Run the executor mount script against disposable loop devices in the Linux VM."""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from test_templates import template


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs)


@unittest.skipUnless(sys.platform == "linux" and os.geteuid() == 0, "requires root in a Linux VM")
class VolumeTests(unittest.TestCase):
    def setUp(self):
        for command in ("losetup", "mkfs.btrfs", "mkfs.ext4", "mount", "wipefs", "lsblk"):
            if not shutil.which(command):
                self.skipTest(f"missing {command}")
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        backing = self.home / "disk"
        with backing.open("wb") as file:
            file.truncate(160 * 1024 * 1024)
        self.device = run("losetup", "--find", "--show", str(backing)).stdout.strip()
        self.addCleanup(lambda: run("losetup", "--detach", self.device))
        self.mount = self.home / "store"
        self.addCleanup(lambda: subprocess.run(["umount", str(self.mount)], capture_output=True))
        code = template("executor.yaml")["Resources"]["Instance"]["Properties"]["UserData"]["Fn::Base64"]["Fn::Sub"]
        script = re.search(r"cat > /usr/local/sbin/layr-data <<'EOF'\n(.*?)\nEOF", code, re.S).group(1)
        # Only the mount destination and the input volume ID change. Use a fake serial lookup.
        script = script.replace("${DataVolume}", "vol-test").replace("/var/lib/layr", str(self.mount))
        self.script = self.home / "layr-data"
        self.script.write_text(script)
        tools = self.home / "bin"
        tools.mkdir()
        lsblk = tools / "lsblk"
        lsblk.write_text(f'''#!/bin/sh
if [ "$1" = -dpno ]; then
  printf '%s voltest\\n' {self.device}
else
  exec /usr/bin/lsblk "$@"
fi
''')
        lsblk.chmod(0o755)
        self.env = {**os.environ, "PATH": str(tools) + ":" + os.environ["PATH"]}

    def test_empty_disk_mount_and_repeat_preserve_data(self):
        run("bash", str(self.script), env=self.env)
        self.assertEqual(run("findmnt", "-nro", "FSTYPE", "-T", str(self.mount)).stdout.strip(), "btrfs")
        before = run("blkid", "-s", "UUID", "-o", "value", self.device).stdout.strip()
        sentinel = self.mount / "keep"
        sentinel.write_text("retained")
        run("bash", str(self.script), env=self.env)
        self.assertEqual(sentinel.read_text(), "retained")
        self.assertEqual(run("blkid", "-s", "UUID", "-o", "value", self.device).stdout.strip(), before)
        run("umount", str(self.mount))
        run("bash", str(self.script), env=self.env)
        self.assertEqual(sentinel.read_text(), "retained")

    def test_other_filesystem_is_rejected_without_format(self):
        run("mkfs.ext4", "-F", self.device)
        result = subprocess.run(["bash", str(self.script)], env=self.env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(run("blkid", "-s", "TYPE", "-o", "value", self.device).stdout.strip(), "ext4")


if __name__ == "__main__":
    unittest.main()
