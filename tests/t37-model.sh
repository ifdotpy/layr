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

mkdir -p "$T/src/docs" "$T/cli" "$T/model"
printf 'Guide from the encrypted store.\n' > "$T/src/docs/guide.txt"
printf '#!/bin/sh\nexit 0\n' > "$T/src/tool.sh"
chmod +x "$T/src/tool.sh"
printf '\000binary\n' > "$T/src/data.bin"
python3 - "$T/src/large.txt" <<'PYTEXT'
from pathlib import Path
import sys
Path(sys.argv[1]).write_text('x' * (65536 + 1))
PYTEXT
ln -s metric.txt "$T/src/metric-link"
printf '100\n' > "$T/src/metric.txt"
printf 'out/\n' > "$T/src/.gitignore"
cat > "$T/src/layr.toml" <<'TOML'
[probe.metric]
run = ["python3", "probe.py"]
outputs = ["out/metric.json"]
kind = "number"
noise = 0.01
fail_over = 0.2
[probe.screens]
run = ["python3", "screens.py"]
outputs = ["out/screens"]
kind = "screenshots"
fail_over = 0.5
TOML
cat > "$T/src/probe.py" <<'PY'
import json
from pathlib import Path
Path('out').mkdir(exist_ok=True)
Path('out/metric.json').write_text(json.dumps({'value': int(Path('metric.txt').read_text())}))
PY

cat > "$T/src/screens.py" <<'PYSCREENS'
import json, struct, zlib
from pathlib import Path
value = int(Path('metric.txt').read_text())
out = Path('out/screens')
out.mkdir(parents=True, exist_ok=True)
def chunk(tag, data):
    return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data))
header = struct.pack('>IIBBBBB', 2, 2, 8, 6, 0, 0, 0)
pixels = bytes([value, 0, 0, 255] + [0, 0, 0, 255] * 3)
rows = b'\0' + pixels[:8] + b'\0' + pixels[8:]
(out / 'Home@Phone.png').write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))
(out / 'Home@Phone.html').write_text(f'<main nonce="{value}"><p>{value}</p></main>')
(out / 'Native@Phone.xml').write_text(f'<node bounds="[1,2][4,6]" text="{value}"/>')
(out / 'Tree@Phone.json').write_text(json.dumps({'label': str(value), 'frame': [1, 2, 3, 4]}))
(out / 'Text@Phone.txt').write_text(f'{value}\n')
(out / 'Stable@Phone.txt').write_text('Same\n')
PYSCREENS

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
V2=$(state_id head)

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

# Push sends all present commit states, so v1 and v2 are in the pushed store.
# The local snapshot export is an independent second input for the CLI JSON twin.
# Export each version and its own parent from real CLI-created snapshots (modes and bytes).
# These are file inputs, never CLI diff answers. No ref pins are added.
l change show "$CHANGE" --json > "$T/versions.json"
python3 - "$LAYR_ROOT/projects/model-golden/states" "$T/versions.json" "$T/model/interdiff-input.json" <<'PYTREES'
import json, os, stat, sys
from pathlib import Path
root, versions, output = map(Path, sys.argv[1:])
def tree(state):
    if state is None:
        return {}
    source = root / state
    assert source.is_dir(), source
    result = {}
    for folder, dirs, files in os.walk(source, followlinks=False):
        for name in sorted(dirs + files):
            path = Path(folder) / name
            meta = path.lstat()
            if stat.S_ISLNK(meta.st_mode):
                data, mode = os.fsencode(os.readlink(path)), 0o120000
            elif stat.S_ISREG(meta.st_mode):
                data = path.read_bytes()
                mode = 0o100755 if meta.st_mode & 0o111 else 0o100644
            else:
                continue
            result[str(path.relative_to(source))] = {'mode': mode, 'bytes': list(data)}
    return result
rows = []
for version in json.loads(versions.read_text())['versions']:
    state = version['state']
    rows.append({'state': state, 'parent': tree(next(iter(state['parents']), None)), 'tree': tree(state['id'])})
output.write_text(json.dumps(rows))
PYTREES
l change diff "$CHANGE" --from v1 --to v2 --json > "$T/cli/interdiff.json"
l change diff "$CHANGE" --from v2 --to v3 --json > "$T/cli/interdiff-replay.json"

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
PICK_CHANGE=$(l log head -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["change_id"])')
l change show "$PICK_CHANGE" --json > "$T/pick-version.json"
ok 'pick records its actor and exact target head' python3 - "$T/pick-version.json" "$PICK" <<'PYPICK'
import json,sys
row=json.load(open(sys.argv[1]))['versions'][-1]
operation=row['operation']
assert operation['kind']=='pick' and operation['source']==sys.argv[2]
assert operation['target_head']==row['state']['parents'][0]
assert operation['actor']['name'] and operation['target_line']
PYPICK
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

l access grant nobody reader
l protect main --path feature.txt=maintainer
l overlay create secrets --readers nobody -q
l overlay use secrets -q
printf 'private bytes\n' > private.txt
l overlay move secrets private.txt -q
l save -qm overlay-state
has 'the overlay has its reader' nobody "$(l overlay readers secrets)"

# Stored policy evidence must predict the same refusals as the CLI.
l config check.smoke '"false"'
if l ref set env/prod "$V3" --expect "$V3" > "$T/ref-refusal.log" 2>&1; then
    REF_ALLOWED=true
else
    REF_ALLOWED=false
fi
has 'the ref refusal requires the exact configured check' "needs a passing check 'smoke'" "$(cat "$T/ref-refusal.log")"
l config check.denymerge '"false"'
l protect main --check denymerge --path feature.txt=maintainer
MERGE_SOURCE=$(state_id feature)
MERGE_BEFORE=$(state_id head)
if l merge feature -q > "$T/merge-refusal.log" 2>&1; then
    MERGE_ALLOWED=true
else
    MERGE_ALLOWED=false
fi
has 'the merge refusal reports its required check' "check 'denymerge' failed" "$(cat "$T/merge-refusal.log")"
eq 'a refused merge keeps the target head' "$MERGE_BEFORE" "$(state_id head)"
MERGE_CANDIDATE=$(l op log --all -n 100000 --json | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; print(next(r["states"][0]["id"] for r in rows if r["op"]=="merge.candidate" and r["data"]["theirs"]==sys.argv[1]))' "$MERGE_SOURCE")
printf '{"merge":%s,"ref":%s}\n' "$MERGE_ALLOWED" "$REF_ALLOWED" > "$T/cli/merge-refusals.json"

# Bisect stores both executed checks and later reuse without changing the line head.
l bisect run --good "$BASE" --bad "$V3" --json -- sh -c 'test ! -f feature.txt' > "$T/bisect-first.json"
l bisect run --good "$BASE" --bad "$V3" --json -- sh -c 'test ! -f feature.txt' > "$T/bisect-reused.json"
ok 'bisect reuse preserves every check result' python3 - "$T/bisect-first.json" "$T/bisect-reused.json" <<'PYBISECT'
import json, sys
first, second = [json.load(open(path)) for path in sys.argv[1:]]
assert first['first_bad'] == second['first_bad']
assert all(step['reused'] for step in second['steps'])
assert [(s['state'], s['exit']) for s in first['steps']] == [(s['state'], s['exit']) for s in second['steps']]
PYBISECT
no 'invalid bisect bounds are refused' l bisect run --good "$V3" --bad "$BASE" --json -- sh -c 'test ! -f feature.txt'
l bisect list --json > "$T/bisect-history.json"
eq 'invalid bounds add no bisect result' 2 "$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$T/bisect-history.json")"

# The peer transport requires Btrfs. Store and version checks also run on macOS.
if [ "$(uname -s)" = Linux ] && [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    new_fs "$T/peer" 4G
    l remote add peer ssh://fixture/model-golden
    l sync peer --command -q -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer model-golden
fi

l remote add store "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l overlay create hidden-golden --hidden --readers nobody -q
printf 'hidden golden bytes\n' > hidden.txt
l overlay move hidden-golden hidden.txt -q
l save -qm hidden-golden-state
l overlay list --json | python3 -c 'import json,sys; rows=json.load(sys.stdin); print(json.dumps([{"name":r["name"],"hidden":r["hidden"]} for r in rows if r["hidden"]]))' > "$T/cli/hidden-reader.json"
# A parent-only reader has no hidden creation or hidden layer descriptor.
l op log --all -n 100000 --json | python3 -c 'import json,sys; rows=[json.loads(l) for l in sys.stdin]; assert not any("hidden-golden" in json.dumps(r) for r in rows); print("[]")' > "$T/cli/hidden-nonreader.json"
l push store --json > "$T/store-push.json"
ok 'one push sends all three version trees' python3 - "$T/archive" "$V1" "$V2" "$V3" <<'PYPRIOR'
import sys
from pathlib import Path
store = Path(sys.argv[1])
for state in sys.argv[2:]:
    assert any((store / 'states' / state).glob('*.tree.age')), state
PYPRIOR
ok 'overlay state IDs stay in the overlay transfer record' python3 - "$LAYR_ROOT/projects/model-golden" "$T/store-push.json" <<'PYPRIVATE'
import json, sqlite3, sys
from pathlib import Path
project=Path(sys.argv[1])
parent_db=sqlite3.connect(project/'meta/db/layr.sqlite')
overlay_db=sqlite3.connect(project/'meta/overlays/secrets/meta/db/layr.sqlite')
def push(db):
    return json.loads(db.execute("select raw from records where op='push' order by time desc, seq desc limit 1").fetchone()[0])['r']
parent, overlay=push(parent_db), push(overlay_db)
heads={row[0] for row in overlay_db.execute('select head from lines where deleted=0')}
parent_ids=set(parent['data']['state_ids_sent'])
overlay_ids=set(overlay['data']['state_ids_sent'])
assert heads and heads <= overlay_ids
assert not parent_ids & overlay_ids
assert not heads & parent_ids
assert parent['project'] != overlay['project'] and overlay['data']['overlay']=='secrets'
assert parent['data']['overlays']==['secrets']
assert parent['data']['states_sent']==len(parent_ids)
assert overlay['data']['states_sent']==len(overlay_ids)
report=json.load(open(sys.argv[2]))
assert report['states']==parent['data']['states_sent']+overlay['data']['states_sent']
assert report['records']==parent['data']['records_sent']+overlay['data']['records_sent']
assert report['bytes']==parent['data']['bytes_sent']+overlay['data']['bytes_sent']
PYPRIVATE
l gc --remote store --json > "$T/remote-gc.json"
ok 'remote GC keeps all three version trees' python3 - "$T/archive" "$V1" "$V2" "$V3" <<'PYGC'
import sys
from pathlib import Path
for state in sys.argv[2:]:
    assert any((Path(sys.argv[1]) / 'states' / state).glob('*.tree.age')), state
PYGC
ok 'all version trees remain readable after remote GC' l fsck --remote store --files

# Only selectors cross into the model example. No CLI answers enter its input.
python3 - "$T/model/request.json" "$CHANGE" "$V3" "$PROBE_BASE" "$PROBE_HEAD" "$MERGE_SOURCE" "$MERGE_CANDIDATE" <<'PY'
import json, sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({
    'line': 'main', 'change': sys.argv[2], 'ref': 'env/prod',
    'attestation_state': sys.argv[3], 'probe_base': sys.argv[4], 'probe_head': sys.argv[5],
    'merge_source': sys.argv[6], 'merge_candidate': sys.argv[7],
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
l bisect list --json > "$T/cli/bisect.json"
l remote list --json | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["copies"]))' > "$T/cli/copies.json"
ok 'change versions retain explicit amend and replay actors and targets' python3 - "$T/cli/change.json" <<'PYVERSION'
import json, sys
rows = json.load(open(sys.argv[1]))['versions']
assert 'operation' not in rows[0]
assert [row['operation']['kind'] for row in rows[1:]] == ['amend', 'replay']
assert all(row['operation']['actor']['name'] and row['operation']['machine'] and row['operation']['time'] for row in rows[1:])
assert all(row['operation']['target_line'] and row['operation']['target_head'] for row in rows[1:])
assert rows[1]['operation']['source'] == rows[0]['state']['id']
assert rows[2]['operation']['source'] == rows[1]['state']['id']
PYVERSION
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
l probe diff "$PROBE_HEAD" "$PROBE_HEAD" --json > "$T/cli/probe-same.json"
eq 'the fixture has exactly two lines' 2 "$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$T/cli/lines.json")"

# A transfer result is appended after success and travels in the next exchange.
# Capture CLI answers first, then publish exactly that record set for the model.
l push store -q

(cd "$REPO" && cargo run -q -p layr-model --example store_golden -- "$T/archive" "$T/paper" "$T/model")

ok 'page outputs contain verified metadata and text bounds' python3 - "$T/model" <<'PYPAGES'
import json, sys
from pathlib import Path
root = Path(sys.argv[1])
load = lambda name: json.loads((root / (name + '.json')).read_text())
project = load('projects')[0]
assert project['name'] == 'model-golden' and project['default_line'] == 'main'
assert project['counts']['lines'] == 2 and project['counts']['changes'] >= 3
assert any(p['name'] == 'metric' for p in project['probes'])
screens = next(p for p in load('probe-diff')['probes'] if p['probe'] == 'screens')
assert screens['status'] == 'changed' and screens['limit_reasons'] == []
rows = {row['screen']: row for row in screens['changes']}
assert set(rows) == {'Home@Phone', 'Native@Phone', 'Tree@Phone', 'Text@Phone', 'Stable@Phone'}
assert rows['Home@Phone']['changed_pixels'] == 1 and rows['Home@Phone']['total_pixels'] == 4
assert rows['Home@Phone']['changed_share'] == 0.25
assert rows['Home@Phone']['regions'] == [{'x': 0, 'y': 0, 'width': 1, 'height': 1}]
assert rows['Home@Phone']['text_changes'] == 1 and '+110' in rows['Home@Phone']['structure_diff']
assert all(rows[name]['text_changes'] == 1 for name in ('Native@Phone', 'Tree@Phone', 'Text@Phone'))
assert rows['Native@Phone']['structure_regions'] == rows['Tree@Phone']['structure_regions'] == [
    {'x': 1.0, 'y': 2.0, 'width': 3.0, 'height': 4.0, 'change': 'text'}]
assert rows['Text@Phone']['changed_share'] is None
assert rows['Stable@Phone']['structure_diff'] == '' and rows['Stable@Phone']['text_changes'] == 0
same = next(p for p in load('probe-same')['probes'] if p['probe'] == 'screens')
assert same['status'] == 'unchanged' and same['limit_reasons'] == [] and len(same['changes']) == 5
assert all(row['changed_share'] in (0, None) and not row['structure_diff'] for row in same['changes'])
trees = load('trees')
assert any(t['path'] == 'docs' and t['entries'][0]['text'].startswith('Guide') for t in trees)
entries = [e for tree in trees for e in tree['entries']]
assert any(e['path'] == 'tool.sh' and e['mode'] == '100755' for e in entries)
assert any(e['path'] == 'metric-link' and e['kind'] == 'symlink' for e in entries)
assert all('text' not in e for e in entries if e['path'] in ('large.txt', 'data.bin'))
assert any(f['path'] == 'docs/guide.txt' and f['text'].startswith('Guide') for f in load('files'))
diff = load('diff')
assert diff['base'] != diff['head']
assert any(f['path'] == 'feature.txt' and f['status'] == 'add' and f['added'] == 2 and '+version one' in f['unified'] for f in diff['files'])
config = load('config')
assert config['policy']['access']['user:0'] == 'admin'
assert config['path_rules']['main']['feature.txt'] == 'maintainer'
overlay = next(o for o in load('overlays') if o['name'] == 'secrets')
assert overlay['name'] == 'secrets' and overlay['readable'] and overlay['readers'] and len(overlay['managers']) == 1
assert 'private.txt' in overlay['paths']
copy = next(row for row in load('copies') if row['name'] == 'store')
assert copy['name'] == 'store' and copy['address'].startswith('file://')
assert copy['time'] > 0 and copy['machine']
assert copy['last']['result'] == 'success' and copy['last']['states_sent'] == len(copy['last']['state_ids_sent'])
assert copy['last']['overlays']
machine = load('machines')[0]
assert machine['trusted'] and machine['fingerprint'].startswith('sha256:') and len(machine['fingerprint']) == 71
assert len(load('bisect')) == 2
assert all(step['reused'] for step in load('bisect')[0]['steps'])
assert load('merge_status')['state'] and load('merge_status')['already_in_target']
browser = load('merge_status_browser')
assert browser and all(row['target_machine'] and row['allowed'] is None and row['status'] == 'not_decided' for row in browser)
assert all(row['undecided_reasons'] for row in browser)
assert load('merge-status') == browser
assert load('interdiff')['files'] and '+version two' in load('interdiff')['diff']
assert load('interdiff-replay')['diff'] == '' and load('interdiff-replay')['files'] == []
assert load('merge-refusals') == {'merge': False, 'ref': False}
assert len(load('changes')) == project['counts']['changes']
assert len(load('operations-all')) == project['counts']['operations']
PYPAGES

# Export generated test data only when the caller supplies an output folder.
if [ -n "${LAYR_MODEL_FIXTURE_OUT:-}" ]; then
    mkdir -p "$LAYR_MODEL_FIXTURE_OUT"
    cp "$T/model/"*.json "$LAYR_MODEL_FIXTURE_OUT/"
fi
if [ -n "${LAYR_MODEL_STORE_OUT:-}" ]; then
    mkdir -m 700 -p "$LAYR_MODEL_STORE_OUT"
    test ! -e "$LAYR_MODEL_STORE_OUT/store"
    test ! -e "$LAYR_MODEL_STORE_OUT/paper"
    cp -R "$T/archive" "$LAYR_MODEL_STORE_OUT/store"
    install -m 600 "$T/paper" "$LAYR_MODEL_STORE_OUT/paper"
fi

if python3 - "$T/cli" "$T/model" "$T/counts" <<'PY'
import difflib, json, sys
from pathlib import Path

cli, model, counts = map(Path, sys.argv[1:])
names = ('lines', 'log', 'log-auto', 'change', 'refs', 'ref-log', 'queue', 'attestations', 'operations', 'probe-diff', 'probe-same', 'merge-refusals', 'interdiff', 'interdiff-replay', 'copies', 'bisect', 'hidden-reader', 'hidden-nonreader')
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
