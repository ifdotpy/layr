"""Concurrent first use must publish one immutable machine identity."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import threading

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()
env = dict(os.environ, LAYR_DIRECT="1")
for key in ("LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE", "LAYR_ROOT"):
    env.pop(key, None)


def run(store, *args):
    return subprocess.run([binary, "--root", str(store), *args], env=env,
                          capture_output=True, text=True, timeout=90)


def concurrent_init(case):
    store = root / "store" / ("identity-" + case)
    if case != "fresh":
        store.mkdir()
        (store / "store-version.json").write_text(json.dumps({"format": 1, "version": "0.4.0", "build": "crash-fixture"}))
    expected_id = None
    if case == "partial":
        machine = store / "machine"
        machine.mkdir()
        expected_id = "019a25f0-0000-7000-8000-000000000001"
        (machine / "id").write_text(expected_id)
        (machine / "name").write_text("existing-machine-name")
    start = threading.Barrier(24)

    def init(number):
        start.wait(timeout=30)
        return run(store, "init", f"p{number}", "-q")

    with ThreadPoolExecutor(max_workers=24) as pool:
        results = list(pool.map(init, range(24)))
    assert all(result.returncode == 0 for result in results), [(r.returncode, r.stderr) for r in results if r.returncode]
    machine_id = (store / "machine/id").read_text().strip()
    public_ids = set()
    failures = []
    for number in range(24):
        name = f"p{number}"
        records = run(store, "--project", name, "op", "export")
        assert records.returncode == 0, records.stderr
        public_ids.update(json.loads(line)["r"]["machine"] for line in records.stdout.splitlines())
        changed = run(store, "--project", name, "line", "new", "topic", "main", "-q")
        checked = run(store, "--project", name, "fsck")
        if changed.returncode or checked.returncode:
            failures.append((number, changed.returncode, checked.returncode))
    assert not failures, f"post-init write/fsck failures={len(failures)}, machine IDs={len(public_ids)}"
    assert public_ids == {machine_id}, f"records use {len(public_ids)} machine IDs"
    if expected_id:
        assert machine_id == expected_id, "initialization replaced an existing machine ID"
        assert (store / "machine/name").read_text() == "existing-machine-name"
    assert not list((store / "machine").glob(".identity-*")), "initialization left a temporary identity file"


failed = 0
for case in ("fresh", "marker", "partial"):
    try:
        concurrent_init(case)
        print(f"PASS 24 concurrent initializations preserve one identity ({case})", flush=True)
    except Exception as error:
        failed += 1
        print(f"FAIL machine identity ({case}): {error!r}", flush=True)
print(f"t55-machine-identity.sh: {3 - failed} passed, {failed} failed")
sys.exit(bool(failed))
