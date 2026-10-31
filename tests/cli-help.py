"""Help and unknown commands must not open or change a local store."""
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()
source = Path(__file__).resolve().parent.parent
dispatch = (source / "src/cmd/mod.rs").read_text().split("pub fn run(ctx:", 1)[1].split("#[cfg(test)]", 1)[0]
arms = re.findall(r'^\s*((?:"[^"]+"\s*\|\s*)*"[^"]+")\s*=>', dispatch, re.M)
commands = {name for arm in arms for name in re.findall(r'"([^"]+)"', arm)}
commands.update(("person", "mount", "daemon", "retest"))
existing = root / "existing"
(existing / "machine").mkdir(parents=True)
(existing / "machine/id").write_text("test-machine")
(existing / "machine/name").write_text("test-machine")
(existing / "machine/key").write_text("0" * 64)
(existing / "projects").mkdir(mode=0o700)

def fingerprint(store):
    if not store.exists():
        return None
    paths = [store, *sorted(store.rglob("*"))]
    return [(str(path.relative_to(store)), path.stat().st_mode, path.stat().st_mtime_ns,
             path.stat().st_ctime_ns, hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None)
            for path in paths]

checks = 0
for store in (root / "absent", existing):
    before = fingerprint(store)
    env = dict(os.environ, LAYR_ROOT=str(store), LAYR_DIRECT="1", LAYR_SOCKET=str(root / "absent-socket"))
    for command in sorted(commands):
        for flag in ("--help", "-h"):
            if command == "grep" and flag == "-h":
                continue
            result = subprocess.run([binary, command, flag], cwd=root, env=env, capture_output=True, text=True)
            assert result.returncode == 0 and "usage: layr" in result.stdout, (command, flag, result.returncode, result.stderr)
            assert fingerprint(store) == before, (command, flag, "the store changed")
            checks += 1
    result = subprocess.run([binary, "unknown-command"], cwd=root, env=env, capture_output=True, text=True)
    assert result.returncode == 1 and "not a layr command" in result.stderr, result
    assert fingerprint(store) == before, "an unknown command changed the store"
    checks += 1
    result = subprocess.run([binary, "unknown-command", "--", "--help"], cwd=root, env=env, capture_output=True, text=True)
    assert result.returncode == 1 and not result.stdout, result
    assert fingerprint(store) == before, "help after -- changed the store"
    checks += 1
grep_root = root / "store"
grep_source = root / "grep-source"
grep_source.mkdir()
(grep_source / "file").write_text("needle\n")
env = dict(os.environ, LAYR_ROOT=str(grep_root), LAYR_DIRECT="1")
env.pop("LAYR_SOCKET", None)
subprocess.run([binary, "init", "grep-fixture", "--from", str(grep_source), "-q"], env=env, check=True, capture_output=True)
work = grep_root / "projects/grep-fixture/lines/main"
result = subprocess.run([binary, "grep", "-h", "needle"], cwd=work, env=env, capture_output=True, text=True)
assert result.returncode == 0 and result.stdout == "needle\n", (result.returncode, result.stdout, result.stderr)
checks += 1
print(f"t57-cli-help.sh: {checks} passed, 0 failed")
