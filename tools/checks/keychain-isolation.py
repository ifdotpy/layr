#!/usr/bin/env python3
"""Run disk tests with private Keychains and check the user's Keychain state."""
import ctypes
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile

if sys.platform != "darwin" or os.getuid() == 0:
    raise SystemExit("Run this check as a non-root macOS user.")

security = ctypes.CDLL("/System/Library/Frameworks/Security.framework/Security")
foundation = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
security.SecKeychainCopyDefault.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
security.SecKeychainGetStatus.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
foundation.CFRelease.argtypes = [ctypes.c_void_p]
foundation.CFRelease.restype = None


def snapshot():
    key = ctypes.c_void_p()
    if security.SecKeychainCopyDefault(ctypes.byref(key)) != 0:
        raise RuntimeError("Cannot read the default Keychain.")
    try:
        status = ctypes.c_uint32()
        if security.SecKeychainGetStatus(key, ctypes.byref(status)) != 0:
            raise RuntimeError("Cannot read the default Keychain status.")
        return {
            "default": subprocess.check_output(
                ["/usr/bin/security", "default-keychain", "-d", "user"], text=True
            ).strip(),
            "search_list": subprocess.check_output(
                ["/usr/bin/security", "list-keychains", "-d", "user"], text=True
            ).strip(),
            "status_flags": status.value,
        }
    finally:
        foundation.CFRelease(key)


root = pathlib.Path(tempfile.mkdtemp(prefix="lk.", dir="/tmp"))
env = os.environ.copy()
env.update(TMPDIR=str(root), LAYR_ROOT=str(root / "store"))
env.pop("LAYR_SOCKET", None)
before = snapshot()
log_path = root / "disk-tests.log"
with log_path.open("w") as log:
    result = subprocess.run(
        ["cargo", "test", "--locked", "--bin", "layr", "disk::", "--", "--test-threads=1"],
        env=env, stdout=log, stderr=subprocess.STDOUT,
    )
after = snapshot()
output = log_path.read_text()
required = [
    "disk::catalog::tests::status_reports_the_keychain_and_service_without_the_identity",
    "disk::keystore::tests::login_keychain_roundtrip",
    "disk::keystore::tests::test_keychain_is_removed_after_a_panic",
    "disk::tests::keychain_lock_hides_files_and_blocks_webdav_until_unlock",
]
missing = [name for name in required if f"test {name} ... ok" not in output]
summary = re.search(r"test result: ok\. ([0-9]+) passed; 0 failed; 0 ignored;", output)
report = {
    "uid": os.getuid(),
    "test_exit": result.returncode,
    "passed": int(summary[1]) if summary else 0,
    "missing_required_tests": missing,
    "default_unchanged": before["default"] == after["default"],
    "search_list_unchanged": before["search_list"] == after["search_list"],
    "status_flags_before": before["status_flags"],
    "status_flags_after": after["status_flags"],
    "remaining_test_keychain_dirs": [str(p) for p in root.glob("layr-test-keychain-*")],
    "tmpdir": str(root),
    "log": str(log_path),
}
print(json.dumps(report))
if result.returncode != 0:
    raise SystemExit(result.returncode)
if not summary or not report["passed"] or missing:
    raise SystemExit("The required Keychain tests did not all pass.")
if before != after:
    raise SystemExit("The default Keychain, search list or status changed.")
if report["remaining_test_keychain_dirs"]:
    raise SystemExit("Temporary test Keychains remain.")
