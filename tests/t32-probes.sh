#!/bin/bash
# Explicit probe execution, memoization, and stored behavior comparisons.
. "$(dirname "$0")/lib.sh"
set -e
mkdir -p "$T/src"
printf 'old\n' > "$T/src/value.txt"
printf '100\n' > "$T/src/metric.txt"
printf 'out/\nattempt\n' > "$T/src/.gitignore"
cat > "$T/src/layr.toml" <<'TOML'
[probe.files]
run = ["python3", "probe.py", "files"]
outputs = ["out/files"]
[probe.structured]
run = ["python3", "probe.py", "json"]
outputs = ["out/data.json"]
kind = "json"
[probe.metric]
run = ["python3", "probe.py", "number"]
outputs = ["out/number.json"]
kind = "number"
noise = 0.05
[probe.failure]
run = ["python3", "probe.py", "failure"]
outputs = ["out/failure.txt"]
[probe.link]
run = ["sh", "-c", "mkdir -p out; ln -s /etc/passwd out/link"]
outputs = ["out/link"]
TOML
cat > "$T/src/probe.py" <<'PY'
import json, os, pathlib, sys
assert 'PROBE_TOKEN' not in os.environ, 'caller environment leaked'
root=pathlib.Path('out'); root.mkdir(exist_ok=True)
kind=sys.argv[1]
value=pathlib.Path('value.txt').read_text()
if kind=='files':
    d=root/'files'; d.mkdir(exist_ok=True)
    (d/'text').write_text(value)
    (d/'binary').write_bytes(b'\0'+value.encode())
elif kind=='json':
    (root/'data.json').write_text(json.dumps({'nested':{'value':value.strip()},'unchanged':1}))
elif kind=='number':
    (root/'number.json').write_text(json.dumps({'time':float(pathlib.Path('metric.txt').read_text())}))
else:
    (root/'failure.txt').write_text('failed')
    sys.exit(7)
print('probe stdout must not break JSON')
PY
l init probes --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/probes/lines/main"
BASE=$(l rev-parse main)
BEFORE=$(l probe diff "$BASE" "$BASE" --json)
has "diff reports probes that were not run" 'not run' "$BEFORE"
eq "diff creates no derived refs" '[]' "$(l ref list derived/probe-v1/ --json)"
for NAME in files structured metric; do
    RESULT=$(PROBE_TOKEN=must-not-leak l probe run main --probe "$NAME" --json)
    ok "explicit $NAME run records a result" python3 -c 'import json,sys; r=json.loads(sys.argv[1])[0]; assert r["exit"]==0 and not r["reused"]' "$RESULT"
done
REFS=$(l ref list derived/probe-v1/ --json)
RESULT=$(l probe run main --probe files --json)
ok "the exact argv and subject reuse the output state" python3 -c 'import json,sys; assert json.loads(sys.argv[1])[0]["reused"]' "$RESULT"
eq "reuse does not create a second derived state" "$REFS" "$(l ref list derived/probe-v1/ --json)"
ok "outputs have derived kind, context, source, and no source history" python3 -c 'import json,sqlite3,sys; d=sqlite3.connect(sys.argv[1]); refs=json.loads(sys.argv[2]); rows=[json.loads(d.execute("select rec from states where id=?",(r["target"],)).fetchone()[0]) for r in refs]; assert len(rows)==3; assert all(r["kind"]=="derived" and r["context"].startswith("probe-v1/") and r["derived_from"]==sys.argv[3] and not r["parents"] and not r.get("line") for r in rows)' "$LAYR_ROOT/projects/probes/meta/db/layr.sqlite" "$REFS" "$BASE"
l gc -q
ok "derived refs retain outputs through GC" python3 -c 'import json,pathlib,sys; assert all((pathlib.Path(sys.argv[1])/r["target"]).is_dir() for r in json.loads(sys.argv[2]))' "$LAYR_ROOT/projects/probes/states" "$REFS"
no "failed probes return failure" l probe run main --probe failure --json
FAILED=$(l probe run main --probe failure --json || true)
ok "failed results retain their exit code and are reused" python3 -c 'import json,sys; r=json.loads(sys.argv[1])[0]; assert r["exit"]==7 and r["reused"]' "$FAILED"
no "output links cannot read a root-only path" l probe run main --probe link
no "unknown probes fail clearly" l probe run main --probe absent
l branch feature main >/dev/null
cd ../feature
printf 'new\n' > value.txt
printf '110\n' > metric.txt
l add -A
l commit -qm feature
HEAD=$(l rev-parse HEAD)
CHANGE=$(l change list --json | python3 -c 'import json,sys; v=json.load(sys.stdin); print(next(r["change_id"] for r in v if r["message"]=="feature"))')
SHOW=$(l change show "$CHANGE" --json)
has "change show has stored behavior without execution" 'not run' "$SHOW"
printf 'dirty\n' > value.txt
for NAME in files structured metric; do l probe run "$HEAD" --probe "$NAME" --json >/dev/null; done
DELTA=$(l probe diff "$BASE" "$HEAD" --json)
ok "text output uses the committed revision and binary output uses hashes" python3 -c 'import json,sys; rows=json.loads(sys.argv[1])["probes"]; f=next(r for r in rows if r["probe"]=="files"); assert f["status"]=="changed"; text=next(c for c in f["changes"] if c["kind"]=="text")["diff"]; assert "+new" in text and "dirty" not in text; binary=next(c for c in f["changes"] if c["kind"]=="binary"); assert binary["before"]["sha256"]!=binary["after"]["sha256"]' "$DELTA"
ok "JSON output reports a structural key path" python3 -c 'import json,sys; r=next(r for r in json.loads(sys.argv[1])["probes"] if r["probe"]=="structured"); assert r["changes"][0]["changes"][0]["key"]=="/nested/value"' "$DELTA"
ok "numeric output reports relative change" python3 -c 'import json,sys; r=next(r for r in json.loads(sys.argv[1])["probes"] if r["probe"]=="metric"); assert r["changes"][0]["changes"][0]["relative_change"]==0.1' "$DELTA"
SHOW=$(l change show "$CHANGE")
has "the change includes a Behavior section" 'Behavior' "$SHOW"
has "the change includes its stored text diff" '+new' "$SHOW"
UNTOUCHED=$(l ref list derived/probe-v1/ --json)
l change show "$CHANGE" --json >/dev/null
l probe diff "$BASE" "$HEAD" >/dev/null
eq "behavior reads never create probe results" "$UNTOUCHED" "$(l ref list derived/probe-v1/ --json)"
printf '104\n' > metric.txt
l add metric.txt
l commit -qm noise
QUIET=$(l rev-parse HEAD)
l probe run "$QUIET" --probe metric >/dev/null
NOISE=$(l probe diff "$BASE" "$QUIET" --json)
ok "the noise threshold suppresses a four-percent metric change" python3 -c 'import json,sys; r=next(r for r in json.loads(sys.argv[1])["probes"] if r["probe"]=="metric"); assert r["status"]=="unchanged" and not r["changes"]' "$NOISE"
finish
