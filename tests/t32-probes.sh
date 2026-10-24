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
[probe.identity]
run = ["python3", "probe.py", "identity"]
outputs = ["out/uid.json"]
kind = "json"
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
elif kind=='identity':
    (root/'uid.json').write_text(json.dumps({'uid':os.getuid()}))
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
ok "the rebuilt binary reports source availability" python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["base_source"]==r["head_source"]=="available"' "$BEFORE"
eq "diff creates no derived refs" '[]' "$(l ref list derived/probe-v1/ --json)"
# A generic check with equal argv does not have probe outputs.
l try -q main -- python3 probe.py files >/dev/null
for NAME in files structured metric; do
    RESULT=$(PROBE_TOKEN=must-not-leak l probe run main --probe "$NAME" --json)
    ok "explicit $NAME run records a result" python3 -c 'import json,sys; r=json.loads(sys.argv[1])[0]; assert r["exit"]==0 and not r["reused"]' "$RESULT"
done
SUBJECT=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])[0]["subject"]["id"])' "$RESULT")
REFS=$(l ref list derived/probe-v1/ --json)
RESULT=$(l probe run main --probe files --json)
ok "the exact argv and subject reuse the output state" python3 -c 'import json,sys; assert json.loads(sys.argv[1])[0]["reused"]' "$RESULT"
eq "reuse does not create a second derived state" "$REFS" "$(l ref list derived/probe-v1/ --json)"
ok "outputs have derived kind, context, source, and no source history" python3 -c 'import json,sqlite3,sys; d=sqlite3.connect(sys.argv[1]); refs=json.loads(sys.argv[2]); rows=[json.loads(d.execute("select rec from states where id=?",(r["target"],)).fetchone()[0]) for r in refs]; assert len(rows)==3; assert all(r["kind"]=="derived" and r["context"].startswith("probe-v1/") and r["derived_from"]==sys.argv[3] and not r["parents"] and not r.get("line") for r in rows)' "$LAYR_ROOT/projects/probes/meta/db/layr.sqlite" "$REFS" "$SUBJECT"
l gc -q
ok "derived refs retain outputs through GC" python3 -c 'import json,pathlib,sys; assert all((pathlib.Path(sys.argv[1])/r["target"]).is_dir() for r in json.loads(sys.argv[2]))' "$LAYR_ROOT/projects/probes/states" "$REFS"
l attest revoke main >/dev/null
REVOKED=$(l probe run main --probe files --json)
ok "a revoked probe attestation cannot reuse its output" python3 -c 'import json,sys; assert not json.loads(sys.argv[1])[0]["reused"]' "$REVOKED"
for NAME in structured metric; do l probe run main --probe "$NAME" >/dev/null; done
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
has "the change includes its stored text summary" 'out/files/text   +1 −1 lines' "$SHOW"
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
# Exact shared views are valid probe inputs; private layers cannot enter shared outputs.
# The project must have a reader other than root, which can read every overlay.
l access grant '*' reader
l restore value.txt
l overlay create private -q
l overlay use private -q
PRIVATE_VIEW=$(l view save --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
PRIVATE_ERROR=$(l probe run "views/$PRIVATE_VIEW" --probe files 2>&1 || true)
has "private view output refusal names the overlay" "private overlay 'private'" "$PRIVATE_ERROR"
l overlay create public --readers '*' -q
l overlay use public -q
printf 'view\n' > value.txt
l overlay move public value.txt
l add -A
l commit -qm view-config
VIEW_STATE=$(l rev-parse HEAD)
VIEW_CHANGE=$(l change list --json | python3 -c 'import json,sys; print(next(r["change_id"] for r in json.load(sys.stdin) if r["message"]=="view-config"))')
VIEW=$(l view save --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
VIEW_RUN=$(l probe run "views/$VIEW" --probe files --json)
VIEW_OUTPUT=$(python3 -c 'import json,sys; r=json.loads(sys.argv[1])[0]; assert r["subject"]=={"kind":"view","id":sys.argv[2]}; print(r["output_state"])' "$VIEW_RUN" "$VIEW")
eq "the probe sees files from the exact overlay state" 'view' "$(cat "$LAYR_ROOT/projects/probes/states/$VIEW_OUTPUT/out/files/text")"
ok "view output provenance contains the exact ViewId" python3 -c 'import json,sqlite3,sys; d=sqlite3.connect(sys.argv[1]); s=json.loads(d.execute("select rec from states where id=?",(sys.argv[2],)).fetchone()[0]); assert s["derived_from"]==sys.argv[3] and s["kind"]=="derived"' "$LAYR_ROOT/projects/probes/meta/db/layr.sqlite" "$VIEW_OUTPUT" "$VIEW"
printf 'later\n' > value.txt
l probe run "views/$VIEW" --probe structured --json >/dev/null
VIEW_DIFF=$(l probe diff "views/$VIEW" "views/$VIEW" --json)
ok "an exact view remains independent of working overlay edits" python3 -c 'import json,sys; r=next(r for r in json.loads(sys.argv[1])["probes"] if r["probe"]=="files"); assert r["status"]=="unchanged"' "$VIEW_DIFF"
VIEW_SHOW=$(l change show "$VIEW_CHANGE" --json)
ok "change show lists view results instead of selecting a view" python3 -c 'import json,sys; b=json.loads(sys.argv[1])["behavior"]; assert sys.argv[2] in b["head_views"]' "$VIEW_SHOW" "$VIEW"
has "human change show gives the explicit view diff command" 'use layr probe diff views/<a> views/<b>' "$(l change show "$VIEW_CHANGE")"
# A ViewId is provenance, not a source retention root.
l config retention.auto 0
l config retention.hours 0
l config undo.days 0
printf 'collect me\n' > source-only.txt
l save -q
AUTO=$(l rev-parse 'HEAD@{0}')
l reset --hard "$AUTO" >/dev/null
COLLECT_VIEW=$(l view save --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
l probe run "views/$COLLECT_VIEW" --probe files --json >/dev/null
l reset --hard "$VIEW_STATE" >/dev/null
l gc -q
no "probe ViewId provenance does not retain source snapshots" l show "$AUTO":source-only.txt
COLLECTED=$(l probe diff "views/$COLLECT_VIEW" "views/$COLLECT_VIEW" --json)
ok "stored outputs still compare after source collection" python3 -c 'import json,sys; b=json.loads(sys.argv[1]); assert b["base_source"]==b["head_source"]=="source collected"; assert next(r for r in b["probes"] if r["probe"]=="files")["status"]=="unchanged"; assert next(r for r in b["probes"] if r["probe"]=="identity")["status"]=="not run"' "$COLLECTED"
COLLECT_ERROR=$(l probe run "views/$COLLECT_VIEW" --probe identity 2>&1 || true)
has "an absent result reports its collected source" 'source collected' "$COLLECT_ERROR"
# The daemon runs repository code with the requesting user's identity.
id layr-probe-user >/dev/null 2>&1 || useradd -m layr-probe-user
l access grant layr-probe-user reader
chmod 755 "$T"
cp "$LAYR" "$T/layr"
chmod 755 "$T/layr"
export LAYR_SOCKET="$T/layr.sock"
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill ${DPID:-} 2>/dev/null || true; cleanup' EXIT
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep .1; done
as_probe_reader() { (cd / && runuser -u layr-probe-user -- env LAYR_SOCKET="$LAYR_SOCKET" PROBE_TOKEN=must-not-leak "$T/layr" --project probes --line main "$@"); }
IDENTITY=$(as_probe_reader probe run main --probe identity --json)
OUTPUT=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])[0]["output_state"])' "$IDENTITY")
eq "the probe runs as the requesting reader" "$(id -u layr-probe-user)" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["uid"])' "$LAYR_ROOT/projects/probes/states/$OUTPUT/out/uid.json")"
l access deny layr-probe-user try
no "the try access rule also applies to stored probe reuse" as_probe_reader probe run main --probe identity --json
finish
