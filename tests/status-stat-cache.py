#!/usr/bin/env python3
# ci: macos overlayfs
"""Validate cached CLI status against full comparisons and count real blob hashes."""
import errno
import json
import mmap
import os
from pathlib import Path
import subprocess
import sys
import time

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve() / "status-cache"
root.mkdir()
source = root / "source"
source.mkdir()
for i in range(30):
    (source / f"f{i}").write_text(f"file {i:02}\n")
(source / "gone").write_text("remove\n")
env = dict(os.environ, LAYR_ROOT=str(root / "store"), LAYR_DIRECT="1", LAYR_SOCKET="/nonexistent/status-cache.sock")
for key in ("LAYR_PROJECT", "LAYR_LINE", "LAYR_NO_STATUS_CACHE", "LAYR_TRACE"):
    env.pop(key, None)
work = root / "store/projects/cache/lines/main"


def run(*args, cwd=root, extra=None):
    p = subprocess.run([binary, *args], cwd=cwd, env=dict(env, **(extra or {})), capture_output=True, text=True, timeout=120)
    assert p.returncode == 0, (args, p.stdout, p.stderr)
    return p


def status(**extra):
    return run("status", "--porcelain", "--ignored", "--untracked-files=all", cwd=work, extra=extra)


def same(label, hashes=None):
    fast = status(LAYR_TRACE="1")
    full = status(LAYR_NO_STATUS_CACHE="1")
    assert fast.stdout == full.stdout, (label, fast.stdout, full.stdout, fast.stderr)
    actual = fast.stderr.count("compare file hash ")
    if hashes is not None:
        assert actual == hashes, (label, actual, hashes, fast.stderr)
    print("PASS", label, flush=True)
    return fast


run("init", "cache", "--no-objects", "--from", str(source), "-q")
run("protect", "main", "--remove", cwd=work)
# Retire native history so unprivileged Linux status does not require btrfs send.
# The working tree is hidden from migration's live-line retention rule.
hidden = root / "hidden"
overlayfs = os.environ.get("LAYR_TEST_BACKEND") == "overlayfs" or bool(os.environ.get("LAYR_OVERLAYFS_TEST_ROOT"))
try:
    work.rename(hidden)
    moved = True
except OSError as error:
    # A working line is a mount on OverlayFS (EBUSY): migration supports its comparisons there.
    if error.errno not in (errno.EBUSY, errno.EXDEV):
        raise
    moved = False
try:
    run("--project", "cache", "migrate")
    if sys.platform == "linux" and not overlayfs:
        for name in ("states", "meta/scan", "meta/work"):
            folder = root / "store/projects/cache" / name
            if folder.exists():
                for path in folder.iterdir():
                    if path.is_dir() and path.stat().st_ino == 256:
                        subprocess.run(["btrfs", "property", "set", "-ts", str(path), "ro", "false"], check=True, capture_output=True)
    run("--project", "cache", "migrate", "--delete-old")
finally:
    if moved:
        hidden.rename(work)

# A settled initial scan is deliberate: files in the cache's clock tick must be rehashed.
time.sleep(1.1)
assert same("first comparison").stdout == ""
# Background work after the migration (owner normalization) can touch entries within the first scan's
# clock tick: those stay racy once. A comparison a tick later saves a settled cache.
time.sleep(1.1)
assert same("settled comparison").stdout == ""
assert same("second clean status hashes zero files", hashes=0).stdout == ""
os.utime(work / "f0", ns=(0, time.time_ns()))
same("same size with new mtime hashes exactly one file", hashes=1)
old = (work / "f1").stat()
(work / "f1").write_text("changed\n")
os.utime(work / "f1", ns=(old.st_atime_ns, old.st_mtime_ns))
assert "f1" in same("same-tick edit with restored mtime").stdout
(work / "gone").unlink()
(work / "new").write_text("untracked\n")
same("creation and deletion")
(work / ".gitignore").write_text("new\n")
same("versioned ignore policy invalidation")
(work / ".git/info").mkdir(parents=True, exist_ok=True)
(work / ".git/info/exclude").write_text("local\n")
(work / "local").write_text("ignored\n")
same("local ignore policy invalidation")
(work / ".git/info/exclude").write_text("")
same("local ignore policy removal")
os.link(work / "f2", work / "hard-link")
with (work / "hard-link").open("r+b") as f:
    f.write(b"X")
same("hard-link write")
(work / "hard-link").unlink()

# The original descriptor is closed before either status. A persistent writable VMA
# alone must prevent trust, including a second write without a new timestamp.
f = (work / "f3").open("r+b")
mapping = mmap.mmap(f.fileno(), 0)
f.close()
try:
    mapping[0:1] = b"Y"
    mapping.flush()
    same("mmap with original descriptor closed")
    mapping[1:2] = b"Z"
    mapping.flush()
    assert "f3" in same("second write through persistent mmap").stdout
finally:
    mapping.close()
same("mapping closed")

# Persisted format/version mismatches and truncation must safely rebuild the cache.
cache = next((root / "store/projects/cache/meta/status").glob("*.cache"))
raw = json.loads(cache.read_bytes())
raw["version"] = "old version"
cache.write_text(json.dumps(raw))
same("version invalidation")
cache.write_bytes(b"truncated")
same("corrupt cache fallback")
print("STATUS CACHE PASS", flush=True)
