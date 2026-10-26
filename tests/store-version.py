"""Check the version guard through the CLI, without using a personal store."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()
env = dict(os.environ, LAYR_DIRECT="1")
for key in ("LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE", "LAYR_ROOT"):
    env.pop(key, None)


def run(store, *args, code=0):
    result = subprocess.run([binary, "--root", str(store), *args], env=env,
                            text=True, capture_output=True, timeout=15)
    assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
    return result


def inspect(store):
    return json.loads(run(store, "store-version", "--json").stdout)


def tree(store):
    return [(str(p.relative_to(store)), p.lstat().st_mode,
             hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else "")
            for p in sorted(store.rglob("*"))]


absent = root / "absent"
report = inspect(absent)
assert report["state"] == "absent" and report["compatible"] and report["store"] is None
assert report["upgrade"] is None
run(absent, "store-version", "--upgrade", code=128)
assert not absent.exists(), "inspection and a refused upgrade create no root"
store = root / "store"
store.mkdir(exist_ok=True)
before = tree(store)
assert inspect(store)["state"] == "empty"
assert inspect(store)["upgrade"] is None
result = run(store, "store-version", "--upgrade", code=128)
assert "the store is empty" in result.stderr
assert tree(store) == before, "an empty-root upgrade creates no marker or lock"
assert tree(store) == before, "inspection creates no marker or lock"
run(store, "--version")
run(store, "--help")
assert tree(store) == before, "help and version do not open the store"
run(store, "projects")
report = inspect(store)
assert report["state"] == "compatible" and report["compatible"]
current = report["binary"]
assert current["format"] == 1 and current["version"] == "0.4.0" and current["build"] != "unknown"
assert report["store"] == current
marker = store / "store-version.json"
assert json.loads(marker.read_text()) == current
before = tree(store)
assert inspect(store) == report
assert tree(store) == before, "inspection preserves every file"
print("PASS read-only inspection and new store marker", flush=True)

# Package and build identity are diagnostic, not format compatibility rules.
other = dict(current, version="0.4.99", build="another-build")
marker.write_text(json.dumps(other))
run(store, "projects")
assert inspect(store)["compatible"]
assert json.loads(marker.read_text()) == other
print("PASS same-format package and build differences remain compatible", flush=True)

for number in (0, 2, 999):
    stored = dict(other, format=number)
    marker.write_text(json.dumps(stored))
    before = tree(store)
    for args in [("projects",), ("status",), ("init", "forbidden", "-q")]:
        result = run(store, *args, code=128)
        for text in (f"format {number}", "format 1", other["version"], other["build"], current["version"], current["build"]):
            assert text in result.stderr, (text, result.stderr)
        assert tree(store) == before, "incompatible access changed the store"
    assert not inspect(store)["compatible"]
    assert inspect(store)["upgrade"] == ({"metadataOnly": True, "fromFormat": 0, "toFormat": 1} if number == 0 else None)
    assert tree(store) == before
    if number > 1:
        run(store, "store-version", "--upgrade", code=128)
        assert tree(store) == before, "upgrade must not downgrade a future store"
print("PASS incompatible reads, writes, and downgrades fail before mutation", flush=True)

legacy = root / "legacy"
legacy.mkdir()
(legacy / "evidence").write_text("legacy bytes\n")
before = tree(legacy)
assert inspect(legacy)["state"] == "legacy"
assert inspect(legacy)["upgrade"] == {"metadataOnly": True, "fromFormat": 0, "toFormat": 1}
result = run(legacy, "projects", code=128)
assert "format 0" in result.stderr and "--upgrade" in result.stderr
assert tree(legacy) == before
run(legacy, "store-version", "--upgrade")
assert inspect(legacy)["compatible"] and (legacy / "evidence").read_text() == "legacy bytes\n"
print("PASS legacy stores require an explicit upgrade", flush=True)

marker.write_text(json.dumps(dict(current, format=0)))
with (store / ".layr-version.lock").open("r+") as lease:
    fcntl.flock(lease, fcntl.LOCK_SH)
    before = tree(store)
    result = run(store, "store-version", "--upgrade", code=128)
    assert "in use" in result.stderr
    assert tree(store) == before
run(store, "store-version", "--upgrade")
assert inspect(store)["store"] == current
print("PASS an active writer blocks a format upgrade", flush=True)

# Revalidate the marker after a blocked lease. The old preflight is insufficient.
with (store / ".layr-version.lock").open("r+") as lease:
    fcntl.flock(lease, fcntl.LOCK_EX)
    proc = subprocess.Popen([binary, "--root", str(store), "projects"], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        try:
            proc.wait(timeout=0.2)
            raise AssertionError("the command did not wait for the format lease")
        except subprocess.TimeoutExpired:
            pass
        marker.write_text(json.dumps(dict(other, format=2)))
        before = tree(store)
        fcntl.flock(lease, fcntl.LOCK_UN)
        stdout, stderr = proc.communicate(timeout=15)
        assert proc.returncode == 128 and "format 2" in stderr, (stdout, stderr)
        assert tree(store) == before
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.communicate()
print("PASS a different format cannot enter after the lease wait", flush=True)

for value in ("not JSON", '{"format":2}', '[]'):
    marker.write_text(value)
    before = tree(store)
    run(store, "projects", code=128)
    run(store, "store-version", "--json", code=128)
    assert tree(store) == before
marker.unlink()
outside = root / "outside-version"
outside.write_text(json.dumps(current))
marker.symlink_to(outside)
run(store, "projects", code=128)
assert outside.read_text() == json.dumps(current)
print("PASS malformed and symlink markers refuse access", flush=True)
print("t53-store-version.sh: 7 passed, 0 failed")
