"""Measure physical space through the public git-backed init command."""
import os
import fcntl
from pathlib import Path
import subprocess
import sys

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()
store = root / "store"
env = dict(os.environ, LAYR_ROOT=str(store))
source = root / "source"
source.mkdir()
filesystem = (subprocess.check_output(["stat", "-f", "-c", "%T", str(store)]).strip()
              if sys.platform == "linux" else b"apfs")
can_clone = True
if sys.platform == "linux":
    probe = store / "clone-source"
    cloned = store / "clone-destination"
    probe.write_bytes(b"clone probe\n")
    try:
        with probe.open("rb") as src, cloned.open("wb") as dst:
            fcntl.ioctl(dst.fileno(), 0x40049409, src.fileno())  # FICLONE
    except OSError:
        can_clone = False
        print("SKIP space assertions: the test filesystem cannot clone files", flush=True)
    finally:
        probe.unlink()
        cloned.unlink(missing_ok=True)


def git(*args):
    return subprocess.check_output(["git", "-C", str(source), *args], env=env).decode().strip()


def layr(cwd, *args):
    result = subprocess.run([binary, *map(str, args)], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=300)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout.strip()


def free_bytes():
    os.sync()
    if filesystem == b"btrfs":
        subprocess.run(["btrfs", "filesystem", "sync", str(store)], check=True,
                       stdout=subprocess.DEVNULL)
    st = os.statvfs(store)
    return st.f_bavail * st.f_frsize


git("init", "-q", "-b", "main")
# A real allocation, with compressible Git objects to isolate the checkout cost.
fixture_mib = int(os.environ.get("LAYR_TEST_IMPORT_MIB", "128")) if can_clone else 1
fixture_label = f"{fixture_mib} MiB"
chunk = (b"layr git import fixture\n" * 65536)[:1024 * 1024]
with (source / "blob").open("wb") as output:
    for _ in range(fixture_mib):
        output.write(chunk)
assert (source / "blob").stat().st_size == fixture_mib * 1024 ** 2
(source / ".gitignore").write_text("untracked-output/\n")
git("add", ".gitignore")
git("add", "-f", "blob")
git("commit", "-qm", "allocation fixture")
if os.environ.get("LAYR_TEST_IMPORT_CASE") != "modified":
    before = free_bytes()
    layr(root, "init", "space", "--from", source, "-q")
    delta = before - free_bytes()
    print(f"git-backed {fixture_label} init: allocated_delta_bytes={delta}", flush=True)
    work = store / "projects/space/lines/main"
    assert layr(work, "rev-parse", "head:blob") == git("rev-parse", "HEAD:blob")
    assert (work / "blob").stat().st_size == fixture_mib * 1024 ** 2
    if can_clone:
        assert delta < fixture_mib * 1024 ** 2 // 8, f"init allocated {delta} bytes for an unchanged {fixture_label} file"
    # A later source write must not change the clone.
    with (work / "blob").open("rb") as imported:
        assert imported.read(4) == chunk[:4]

with (source / "blob").open("r+b") as output:
    output.write(b"edit")

# HEAD needs its own bytes for a modified file. The current bytes must stay shared.
before = free_bytes()
layr(root, "init", "modified-space", "--from", source, "-q")
modified_delta = before - free_bytes()
print(f"modified git-backed {fixture_label} init: allocated_delta_bytes={modified_delta}", flush=True)
modified_work = store / "projects/modified-space/lines/main"
assert layr(modified_work, "rev-parse", "head~1:blob") == git("rev-parse", "HEAD:blob")
with (modified_work / "blob").open("rb") as imported:
    assert imported.read(4) == b"edit"
if can_clone:
    assert modified_delta < fixture_mib * 1024 ** 2 * 9 // 8, (
        f"init allocated {modified_delta} bytes for a modified {fixture_label} file")

source = root / "semantic-source"
source.mkdir()
git("init", "-q", "-b", "main")
original = {
    "same-size": "head\n",
    "longer": "short\n",
    "missing": "deleted\n",
    "executable": "#!/bin/sh\n",
    "regular": "regular\n",
    "parent/file": "nested\n",
    "target": "target\n",
}
for name, content in original.items():
    path = source / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
(source / "executable").chmod(0o755)
(source / "link").symlink_to("target")
git("add", ".")
git("commit", "-qm", "HEAD semantics")
(source / "same-size").write_text("work\n")
(source / "longer").write_text("a longer working file\n")
(source / "missing").unlink()
(source / "executable").chmod(0o644)
(source / "regular").unlink()
(source / "regular").symlink_to("target")
(source / "link").unlink()
(source / "link").write_text("a working regular file\n")
(source / "parent/file").unlink()
(source / "parent").rmdir()
outside = root / "outside"
outside.mkdir()
(outside / "file").write_text("outside\n")
(source / "parent").symlink_to(outside, target_is_directory=True)
layr(root, "init", "semantics", "--from", source, "-q")
work = store / "projects/semantics/lines/main"
assert layr(work, "rev-parse", "head~1^{tree}") == git("rev-parse", "HEAD^{tree}")
for name, content in original.items():
    assert layr(work, "show", "head~1:" + name) == content.strip(), name
assert layr(work, "show", "head~1:link") == "target"
assert (work / "same-size").read_text() == "work\n"
assert (work / "longer").read_text() == "a longer working file\n"
assert not (work / "missing").exists()
assert (work / "executable").stat().st_mode & 0o777 == 0o644
assert (work / "regular").is_symlink()
assert (work / "link").read_text() == "a working regular file\n"
assert (work / "parent").is_symlink()
assert layr(work, "status", "--porcelain") == ""
print(f"t51-git-init-space.sh: {25 if can_clone else 23} passed, 0 failed")
