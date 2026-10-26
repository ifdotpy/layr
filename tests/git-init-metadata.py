"""Exercise folder import rules through the public commands."""
import os
from pathlib import Path
import subprocess
import sys

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()
store = root / "store"
env = dict(os.environ, LAYR_ROOT=str(store))
passed = failed = 0


def git(source, *args):
    return subprocess.check_output(["git", "-C", str(source), *args], env=env).decode().strip()


def layr(cwd, *args):
    result = subprocess.run([binary, *map(str, args)], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=60)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout.strip()


def check(name, action):
    global passed, failed
    try:
        action()
        passed += 1
        print("PASS " + name, flush=True)
    except (AssertionError, OSError) as error:
        failed += 1
        print(f"FAIL {name}: {error}", flush=True)


def tracked_ignored():
    source = root / "ignored-source"
    (source / "ignored").mkdir(parents=True)
    (source / ".gitignore").write_text("ignored/\n")
    (source / "ignored/kept").write_text("HEAD\n")
    (source / "ignored/unchanged").write_text("unchanged\n")
    (source / "ignored/untracked").write_text("exclude\n")
    git(source, "init", "-q", "-b", "main")
    git(source, "add", ".gitignore")
    git(source, "add", "-f", "ignored/kept", "ignored/unchanged")
    git(source, "commit", "-qm", "tracked ignored files")
    (source / "ignored/kept").write_text("work\n")
    layr(root, "init", "ignored", "--from", source, "-q")
    work = store / "projects/ignored/lines/main"
    assert (work / "ignored/kept").read_text() == "work\n"
    assert (work / "ignored/unchanged").read_text() == "unchanged\n"
    assert not (work / "ignored/untracked").exists()
    assert layr(work, "show", "head~1:ignored/kept") == "HEAD"


check("tracked ignored files stay in the import", tracked_ignored)
source = root / "directory-source"
(source / "kept").mkdir(parents=True)
(source / "kept/file").write_text("base\n")
git(source, "init", "-q", "-b", "main")
git(source, "add", ".")
git(source, "commit", "-qm", "directory base")
(source / "empty/nested").mkdir(parents=True)
(source / "empty/nested").chmod(0o701)
(source / "kept").chmod(0o710)
layr(root, "init", "directories", "--from", source, "-q")
work = store / "projects/directories/lines/main"


def empty_directory():
    assert (work / "empty/nested").is_dir(), "the new empty directory is missing"
    assert (work / "empty/nested").stat().st_mode & 0o777 == 0o701


def directory_mode():
    assert (work / "kept").stat().st_mode & 0o777 == 0o710
    assert (work / "kept/file").read_text() == "base\n"
    assert layr(work, "log", "-1", "--format=%s") == "unsaved changes of the imported folder"


check("empty directories survive the import", empty_directory)
check("directory mode changes create a child state", directory_mode)


def plain_directory_mode():
    source = root / "plain-source"
    (source / "sub").mkdir(parents=True)
    (source / "sub/file").write_text("plain\n")
    (source / "sub").chmod(0o710)
    layr(root, "init", "plain", "--from", source, "-q")
    work = store / "projects/plain/lines/main"
    assert (work / "sub").stat().st_mode & 0o777 == 0o710


check("plain folder imports preserve directory modes", plain_directory_mode)
print(f"t52-git-init-metadata.sh: {passed} passed, {failed} failed")
sys.exit(bool(failed))
