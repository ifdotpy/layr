"""Check bounded op-log decoding through the CLI; optionally measure a baseline binary."""
import argparse
import json
import os
from pathlib import Path
import re
import sqlite3
import statistics
import subprocess
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=lambda p: str(Path(p).resolve()))
parser.add_argument("root", type=Path)
parser.add_argument("--baseline", type=lambda p: str(Path(p).resolve()))
parser.add_argument("--records", type=int, default=100)
args = parser.parse_args()
source = (Path(__file__).resolve().parent.parent / "src/db.rs").read_text()
schema = source.split('pub(crate) const SCHEMA: &str = r#"', 1)[1].split('"#;', 1)[0]
version = int(re.search(r"pub const SCHEMA_VERSION: i64 = (\d+);", source)[1])
method = source.split("pub fn log_records(", 1)[1].split("    /// The newest records whose", 1)[0]
query = re.search(r'"SELECT records.raw.*?selected.seq DESC"', method, re.S)[0][1:-1]
project_id, line_id = str(uuid.uuid4()), str(uuid.uuid4())
records = []
for i in range(args.records + 1):
    record = dict(id=str(uuid.uuid4()), machine="", seq=i, time=i // 3, project=project_id,
                  actor=dict(uid=os.getuid(), name="Tester"), states=[], lines={})
    record["op"] = "project" if i == 0 else ("push" if i % 17 == 0 else "sync" if i % 19 == 0 else "save")
    record["data"] = {"id": project_id} if i == 0 else {"line": "other", "note": line_id}
    if i and i % 3 == 0:
        record["data"]["line"] = line_id
    if i and i % 5 == 0:
        record["lines"][line_id] = dict(before=None, after=None)
    records.append(record)


def fixture(binary, name, baseline=False):
    root = args.root / name
    env = dict(os.environ, LAYR_ROOT=str(root), LAYR_DIRECT="1")
    for key in ("LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE"):
        env.pop(key, None)
    subprocess.run([binary, "remote", "self", "--json"], env=env, check=True, capture_output=True, timeout=30)
    machine = (root / "machine/id").read_text().strip()
    project = root / "projects/p"
    work = project / "lines/main"
    work.mkdir(parents=True)
    (work / "fixture").write_text("operation log fixture\n")
    db = project / "meta/db/layr.sqlite"
    db.parent.mkdir(parents=True)
    conn = sqlite3.connect(db)
    ddl = schema
    if baseline:
        ddl = ddl.replace("CREATE INDEX IF NOT EXISTS records_time_seq ON records(time, seq);", "")
        ddl = re.sub(r"-- kind 0:.*?END;", "", ddl, flags=re.S)
    conn.executescript(ddl)
    conn.execute(f"PRAGMA user_version={version - 1 if baseline else version}")
    conn.execute("INSERT INTO lines(id,name,owner,machine,head,time) VALUES(?,?,?,?,?,0)",
                 (line_id, "main", os.getuid(), machine, ""))
    for record in records:
        record = dict(record, machine=machine)
        conn.execute("INSERT INTO records(id,machine,seq,time,op,actor_uid,raw) VALUES(?,?,?,?,?,?,?)",
                     (record["id"], machine, record["seq"], record["time"], record["op"], os.getuid(),
                      json.dumps(dict(r=record, s=""))))
    conn.commit()
    return conn, env, work


def run(binary, env, cwd, n, all_records=False):
    command = [binary, "op", "log", "--json", "-n", str(n)]
    if all_records:
        command.append("--all")
    start = time.perf_counter()
    result = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True, timeout=30)
    elapsed = (time.perf_counter() - start) * 1000
    assert result.returncode == 0, result.stderr
    return [json.loads(row) for row in result.stdout.splitlines()], elapsed


conn, env, work = fixture(args.binary, "after")
for all_records in (False, True):
    expected = [r["id"] for r in reversed(records) if all_records or r["op"] in ("push", "sync")
                or line_id in r["lines"] or r["data"].get("line") == line_id]
    for n in (0, 1, 30, 50, args.records + 10):
        actual, _ = run(args.binary, env, work, n, all_records)
        assert [r["id"] for r in actual] == expected[:max(1, n)], (all_records, n)
# Project-level invocation without a line uses the complete log.
assert run(args.binary, env, work.parent.parent, 1)[0][0]["id"] == records[-1]["id"]
print("PASS CLI limits, ordering, exact associations, global transfers, --all, project context", flush=True)

if args.baseline:
    before, before_env, before_work = fixture(args.baseline, "before", baseline=True)
    for n in (1, 50):
        baseline_output, _ = run(args.baseline, before_env, before_work, n)
        output, _ = run(args.binary, env, work, n)
        # Different local machine identities are the only fixture difference.
        assert [dict(r, machine="") for r in baseline_output] == [dict(r, machine="") for r in output]
        for label, binary, command_env, cwd in (("before", args.baseline, before_env, before_work),
                                                ("after", args.binary, env, work)):
            samples = [run(binary, command_env, cwd, n)[1] for _ in range(5)]
            print(f"{label} -n {n}: median_ms={statistics.median(samples):.3f} samples_ms={samples}")
        counts = {"candidates": 0, "decoded": 0}
        def candidate(value):
            counts["candidates"] += 1
            return value
        def selected(value):
            counts["decoded"] += 1
            return value
        conn.create_function("candidate", 1, candidate)
        conn.create_function("selected", 1, selected)
        instrumented = query.replace("SELECT records.raw", "SELECT selected(records.raw)").replace(
            "SELECT time, seq, record_id FROM record_lines", "SELECT time, seq, candidate(record_id) AS record_id FROM record_lines")
        conn.execute(instrumented, (line_id, n)).fetchall()
        assert counts["decoded"] == n and counts["candidates"] <= n + 2, counts
        old_count = before.execute("SELECT COUNT(*) FROM (SELECT raw FROM records ORDER BY time DESC, seq DESC LIMIT 100000)").fetchone()[0]
        print(f"-n {n}: before returned/decoded={old_count}; after index candidates={counts['candidates']} returned/decoded={counts['decoded']}")
    print("EXPLAIN QUERY PLAN line:")
    for row in conn.execute("EXPLAIN QUERY PLAN " + query, (line_id, 1)):
        print(row[3])
    print("EXPLAIN QUERY PLAN all:")
    for row in conn.execute("EXPLAIN QUERY PLAN SELECT raw FROM records ORDER BY time DESC, seq DESC LIMIT 1"):
        print(row[3])
    before.close()
else:
    # The older JSON remains valid, but decoding it must fail. It is outside the requested window.
    record = dict(records[1], actor=None)
    conn.execute("UPDATE records SET raw=? WHERE id=?", (json.dumps(dict(r=record, s="")), record["id"]))
    conn.commit()
    assert run(args.binary, env, work, 1)[0][0]["id"] == records[-1]["id"]
    assert len(run(args.binary, env, work, 30, True)[0]) == 30
    result = subprocess.run([args.binary, "op", "log", "--all", "-n", str(args.records + 10)],
                            cwd=work, env=env, capture_output=True, text=True, timeout=30)
    assert result.returncode != 0, "the fixture must fail if the older record is decoded"
    print("PASS -n 1 and -n 30 do not decode the complete history", flush=True)
conn.close()
print("t165-op-log-limit.sh: 2 passed, 0 failed")
