#!/bin/bash
# The model reads an encrypted store and must match the CLI JSON.
REPO=$(cd "$(dirname "$0")/.." && pwd)
. "$REPO/tests/lib.sh"
set -e
set -o pipefail

# rev-parse displays state IDs. The model consumes the UUIDs from JSON.
state_id() {
    l log "$1" -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'
}

mkdir -p "$T/src" "$T/cli" "$T/model"
printf '100\n' > "$T/src/metric.txt"
printf 'out/\n' > "$T/src/.gitignore"
cat > "$T/src/layr.toml" <<'TOML'
[probe.metric]
run = ["python3", "probe.py"]
outputs = ["out/metric.json"]
kind = "number"
noise = 0.01
fail_over = 0.2
TOML
cat > "$T/src/probe.py" <<'PY'
import json
from pathlib import Path
Path('out').mkdir(exist_ok=True)
Path('out/metric.json').write_text(json.dumps({'value': int(Path('metric.txt').read_text())}))
PY

l init model-golden --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/model-golden/lines/main"
BASE=$(state_id head)
l line new feature -q
cd ../feature
printf 'version one\n' > feature.txt
l add feature.txt
l save -qm feature-v1
V1=$(state_id head)
CHANGE=$(l change list --json | python3 -c 'import json,sys; print(next(c["change_id"] for c in json.load(sys.stdin) if c["message"] == "feature-v1"))')
l config check.smoke 'test -f metric.txt'
l try -q "$V1" --check smoke
l approve -q "$V1"
printf 'version two\n' >> feature.txt
l save -qam feature-v2 --amend

cd ../main
printf 'main only\n' > main.txt
l add main.txt
l save -qm main-base
cd ../feature
l replay -q main >/dev/null
V3=$(state_id head)
eq 'replay creates the third change version' "$V3" "$(state_id "changes/$CHANGE/v3")"
eq 'the change has exactly three versions' 3 "$(l change show "$CHANGE" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["versions"]))')"
l try -q "$V3" --check smoke
l approve -q "$V3"

cd ../main
printf 'main after replay\n' > divergence.txt
l add divergence.txt
l save -qm main-divergence
l merge --no-ff -q feature >/dev/null
MERGE=$(state_id head)
ok 'the merge state has two parents' python3 -c 'import json,subprocess,sys; state=json.loads(subprocess.check_output([sys.argv[1],"log","-n","1","--json"])); assert len(state["parents"])==2' "$LAYR"
cd ../feature
printf 'picked\n' > picked.txt
l add picked.txt
l save -qm pick-source
PICK=$(state_id head)
cd ../main
l pick -q "$PICK" >/dev/null
l invert head >/dev/null

# Retain an auto state without moving the named head.
NAMED=$(state_id head)
printf 'auto bytes\n' > auto.txt
l save -q --note 'auto note'
AUTO=$(state_id 'head@{0}')
eq 'the auto state keeps the named head' "$NAMED" "$(state_id head)"
l ref set pins/auto "$AUTO" >/dev/null
l park -q -u
l park pop >/dev/null
eq 'park pop restores the auto bytes' 'auto bytes' "$(cat auto.txt)"
l add auto.txt
l save -qm after-park

# A protected ref keeps both transitions and requires an exact old target.
l ref set env/prod "$BASE" >/dev/null
l protect env/prod --update maintainer --check smoke
l ref set env/prod "$V3" --expect "$BASE" >/dev/null
l ref log env/prod --json > "$T/ref-history.json"
eq 'the protected ref has two transitions' 2 "$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$T/ref-history.json")"
for KIND in origin import quarantine; do l attest "$KIND" "$V3" >/dev/null; done
l attest revoke "$V3" >/dev/null
l try -q "$V3" --check smoke
l approve -q "$V3"

# The queue compares stored probe results for the target and the merge result.
cd ../feature
printf '110\n' > metric.txt
l save -qam queue-source
l config queue.probes true
l queue add head --into main >/dev/null
cd ../main
PROBE_BASE=$(state_id head)
l probe run "$PROBE_BASE" --json >/dev/null
l queue run --into main >/dev/null
PROBE_HEAD=$(state_id head)
l probe diff "$PROBE_BASE" "$PROBE_HEAD" --json > "$T/probe-result.json"
ok 'the queue records a successful probe comparison' python3 -c 'import json,subprocess,sys; rows=json.loads(subprocess.check_output([sys.argv[1],"queue","list","--json"])); assert len(rows)==1 and rows[0]["status"]=="done"; assert rows[0]["probes"]["status"]=="passed"' "$LAYR"
ok 'the probe diff contains a numeric change' python3 -c 'import json,sys; b=json.load(open(sys.argv[1])); p=next(p for p in b["probes"] if p["probe"]=="metric"); assert p["status"]=="changed" and p["changes"]' "$T/probe-result.json"

cd ../feature
printf '150\n' > metric.txt
l save -qam queue-over-limit
l queue add head --into main >/dev/null
cd ../main
no 'a probe over its limit fails the queue entry' l queue run --into main
eq 'a failed queue probe keeps the target head' "$PROBE_HEAD" "$(state_id head)"
ok 'the queue stores both probe outcomes' python3 -c 'import json,subprocess,sys; rows=json.loads(subprocess.check_output([sys.argv[1],"queue","list","--json"])); assert [r["status"] for r in rows]==["done","failed"]; assert rows[1]["probes"]["status"]=="failed"' "$LAYR"

l overlay create secrets --readers nobody -q
l overlay use secrets -q
printf 'private bytes\n' > private.txt
l overlay move secrets private.txt -q
l save -qm overlay-state
has 'the overlay has its reader' nobody "$(l overlay readers secrets)"

l remote add store "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push store -q

# Only selectors cross into the model example. No CLI answers enter its input.
python3 - "$T/model/request.json" "$CHANGE" "$V3" "$PROBE_BASE" "$PROBE_HEAD" <<'PY'
import json, sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({
    'line': 'main', 'change': sys.argv[2], 'ref': 'env/prod',
    'attestation_state': sys.argv[3], 'probe_base': sys.argv[4], 'probe_head': sys.argv[5],
}))
PY
l line list --json > "$T/cli/lines.json"
l line list --json feature > "$T/cli/feature.json"
ok 'line JSON has the fields and applies the name filter' python3 -c 'import json,sys; rows=json.load(open(sys.argv[1])); assert len(rows)==1 and rows[0]["name"]=="feature"; assert set(rows[0])=={"id","name","head","owner","machine","path"}' "$T/cli/feature.json"
l log --json > "$T/cli/log.jsonl"
l log --auto --json > "$T/cli/log-auto.jsonl"
python3 - "$T/cli/log-auto.jsonl" "$T/cli/log-auto.json" <<'PY'
import json, sys
from pathlib import Path
rows = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines() if line.strip()]
assert any(state["kind"] == "auto" for state in rows), "the log must include automatic states"
Path(sys.argv[2]).write_text(json.dumps(rows))
PY
python3 - "$T/cli/log.jsonl" "$T/cli/log.json" "$MERGE" <<'PY'
import json, sys
from pathlib import Path
rows = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines() if line.strip()]
assert any(s['id'] == sys.argv[3] and len(s['parents']) == 2 for s in rows), 'the log must contain a merge'
Path(sys.argv[2]).write_text(json.dumps(rows))
PY
l change show "$CHANGE" --json > "$T/cli/change.json"
l ref list --json > "$T/cli/refs.json"
l ref log env/prod --json > "$T/cli/ref-log.json"
l queue list --json > "$T/cli/queue.json"
l attestations "$V3" --json > "$T/cli/attestations.json"
ok 'the fixture retains all six attestation kinds' python3 -c 'import json,sys; rows=json.load(open(sys.argv[1])); assert {r["kind"] for r in rows}=={"check","approval","origin","import","quarantine","revoke"}' "$T/cli/attestations.json"
l op log --json > "$T/cli/operations.jsonl"
python3 - "$T/cli/operations.jsonl" "$T/cli/operations.json" <<'PY'
import json, sys
from pathlib import Path
rows = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines() if line.strip()]
Path(sys.argv[2]).write_text(json.dumps(rows))
PY
l probe diff "$PROBE_BASE" "$PROBE_HEAD" --json > "$T/cli/probe-diff.json"
eq 'the fixture has exactly two lines' 2 "$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$T/cli/lines.json")"

(cd "$REPO" && cargo run -q -p layr-model --example store_golden -- "$T/archive" "$T/paper" "$T/model")

if python3 - "$T/cli" "$T/model" "$T/counts" <<'PY'
import difflib, json, sys
from pathlib import Path

cli, model, counts = map(Path, sys.argv[1:])
names = ('lines', 'log', 'log-auto', 'change', 'refs', 'ref-log', 'queue', 'attestations', 'operations', 'probe-diff')
passed = failed = 0
for name in names:
    try:
        expected = json.loads((cli / (name + '.json')).read_text())
        actual = json.loads((model / (name + '.json')).read_text())
        # The local line path is absent from the store model. Keep all other fields.
        if name == 'lines':
            for row in expected:
                del row['path']
        left = json.dumps(expected, sort_keys=True, indent=2, ensure_ascii=True, allow_nan=False) + '\n'
        right = json.dumps(actual, sort_keys=True, indent=2, ensure_ascii=True, allow_nan=False) + '\n'
        if left != right:
            print('FAIL model ' + name)
            print(''.join(difflib.unified_diff(left.splitlines(True), right.splitlines(True),
                fromfile='CLI/' + name, tofile='model/' + name)), end='')
            failed += 1
        else:
            print('PASS model ' + name)
            passed += 1
    except (OSError, ValueError) as error:
        print('FAIL model ' + name + ': ' + str(error))
        failed += 1
counts.write_text(f'{passed} {failed}\n')
sys.exit(bool(failed))
PY
then
    :
else
    : # Report all JSON differences before finish returns failure.
fi
read -r MODEL_PASS MODEL_FAIL < "$T/counts"
PASS=$((PASS + MODEL_PASS))
FAIL=$((FAIL + MODEL_FAIL))
finish
