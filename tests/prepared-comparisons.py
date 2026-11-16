#!/usr/bin/env python3
"""Count whole comparisons and exercise the prepare/publication boundary."""
import json
import os
from pathlib import Path
import subprocess
import sqlite3
import sys
import time

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve() / "prepared"
root.mkdir()
source = root / "source"
source.mkdir()
(source / ".gitignore").write_text("ignored/\n")
for i in range(30):
    (source / f"f{i}").write_text(f"file {i}\n")
env = dict(os.environ, LAYR_ROOT=str(root / "store"), LAYR_DIRECT="1", LAYR_SOCKET="/nonexistent/prepared.sock")
for key in ("LAYR_PROJECT", "LAYR_LINE", "LAYR_TRACE", "LAYR_NO_STATUS_CACHE"):
    env.pop(key, None)
project = root / "store/projects/p"
work = project / "lines/main"


def run(*args, cwd=work, extra=None):
    result = subprocess.run([binary, *args], cwd=cwd, env=dict(env, **(extra or {})), capture_output=True, text=True, timeout=120)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result


def retire():
    if sys.platform == "linux" and not overlay_backend:
        for name in ("states", "meta/scan", "meta/work"):
            folder = project / name
            if folder.exists():
                for path in folder.iterdir():
                    if path.is_dir() and path.stat().st_ino == 256:
                        subprocess.run(["btrfs", "property", "set", "-ts", str(path), "ro", "false"], check=True, capture_output=True)
    run("line", "list")


def paused(args, marker, change, extra=None, succeeds=True):
    log = root / "paused.log"
    with log.open("w") as output:
        child = subprocess.Popen([binary, *args], cwd=work, env=dict(env, LAYR_TRACE="1", LAYR_TEST_SAVE_PAUSE_MS="1500", **(extra or {})), stdout=output, stderr=output)
        try:
            deadline = time.monotonic() + 30
            while marker not in log.read_text():
                assert child.poll() is None, log.read_text()
                assert time.monotonic() < deadline, log.read_text()
                time.sleep(0.01)
            change()
            assert (child.wait(timeout=120) == 0) == succeeds, log.read_text()
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
    return log.read_text()


run("init", "p", "--from", str(source), "-q", cwd=root)
overlay_backend = (root / "store/.layr-overlayfs").exists()
run("protect", "main", "--remove")
if overlay_backend:
    # Working lines are mountpoints; OverlayFS migration supports their native comparisons.
    run("--project", "p", "migrate", cwd=root)
else:
    # Avoid privileged native btrfs send during the unprivileged fixture migration.
    hidden = root / "hidden"
    work.rename(hidden)
    try:
        run("--project", "p", "migrate", cwd=root)
    finally:
        hidden.rename(work)
retire()
(work / "f0").write_text("staged\n")
run("add", "f0")
retire()
time.sleep(1.1)
result = run("save", "-qm", "saved", extra={"LAYR_TRACE": "1"})
assert result.stderr.count("status full comparison") == 1, result.stderr
assert "saved status used for" in result.stderr, result.stderr
retire()
run("line", "new", "topic")
retire()
topic = work.parent / "topic"
(topic / "f0").write_text("topic\n")
run("add", "f0", cwd=topic)
retire()
run("save", "-qm", "topic", cwd=topic)
retire()
time.sleep(1.1)
result = run("merge", "-q", "topic", extra={"LAYR_TRACE": "1", "LAYR_NO_STATUS_CACHE": "1"})
assert result.stderr.count("status full comparison") == 1, result.stderr
assert (work / "f0").read_text() == "topic\n"
retire()
# Quiet saves retain the warning, including a file changed after preparation.
(work / "f0").write_text("saved again\n")
run("add", "f0")
retire()
def edit_during_save():
    repair = run("fsck", "--repair")
    assert "snapshot without record" not in repair.stdout, repair.stdout
    (work / "f1").write_text("concurrent\n")


result = paused(["save", "-qm", "concurrent"], "prepared named save ready", edit_during_save)
assert "not saved (modified, not added): f1" in result, result
assert "f1" in run("status", "--porcelain").stdout
retire()
# A write after a merge's capture invalidates it and must survive the FF.
run("add", "f1")
retire()
run("save", "-qm", "local")
retire()
run("line", "new", "next")
retire()
next_work = work.parent / "next"
(next_work / "f0").write_text("next\n")
run("add", "f0", cwd=next_work)
retire()
run("save", "-qm", "next", cwd=next_work)
retire()
time.sleep(1.1)
result = paused(["merge", "-q", "next"], "prepared comparison ready", lambda: (work / "f2").write_text("concurrent merge\n"))
assert "prepared comparison invalidated" in result, result
assert (work / "f2").read_text() == "concurrent merge\n"
assert "f2" in run("status", "--porcelain").stdout
# Failure to inspect an ignored folder affects the warning, never the staged save.
(work / "f0").write_text("valid staged save\n")
run("add", "f0")
retire()
ignored = work / "ignored"
ignored.mkdir()
(ignored / "secret").write_text("ignored\n")
ignored.chmod(0)
try:
    run("save", "-qm", "unreadable ignored folder")
finally:
    ignored.chmod(0o700)
assert run("show", "head:f0").stdout == "valid staged save\n"
# An unchanged automatic save still creates a distinct state when explicitly forced.
retire()
first = json.loads(run("save", "--force", "--json").stdout)
retire()
second = json.loads(run("save", "--force", "--json").stdout)
assert first["saved"] and second["saved"] and first["state"] != second["state"], (first, second)
assert run("show", first["state"] + ":f0").stdout == run("show", second["state"] + ":f0").stdout
retire()
if sys.platform == "darwin":
    (work / "f0").write_text("failed preparation\n")
    before = {path for folder in (project / "states", project / "deltas") if folder.exists() for path in folder.iterdir()}
    failure = subprocess.run([binary, "save"], cwd=work, env=dict(env, LAYR_TEST_PREPARE_SAVE_ERROR="1"), capture_output=True, text=True, timeout=120)
    assert failure.returncode != 0 and "prepared save failed after copying" in failure.stderr, failure.stderr
    after = {path for folder in (project / "states", project / "deltas") if folder.exists() for path in folder.iterdir()}
    assert after <= before, (before, after, failure.stderr)
    # An invalidated preparation publishes its retained capture, then allocates a
    # new layer for the safe fallback. Failure of that new copy must clean its ID.
    trace = paused(["save"], "prepare save", lambda: (work / "f1").write_text("late fallback write\n"),
                   extra={"LAYR_TEST_WORKING_SNAPSHOT_ERROR": "1"}, succeeds=False)
    assert "layer over prepared save" in trace and "working snapshot failed after copying" in trace, trace
    with sqlite3.connect(project / "meta/db/layr.sqlite") as db:
        recorded = {row[0] for row in db.execute("SELECT id FROM states")}
    after = {path for folder in (project / "states", project / "deltas") if folder.exists() for path in folder.iterdir()}
    assert not {path for path in after - before if path.name not in recorded}, trace
    assert (work / "f1").read_text() == "late fallback write\n"
print("PREPARED COMPARISONS PASS")
