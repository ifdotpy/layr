#!/bin/bash
# Person requests use real CLI signing and the executor's normal record path.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init requests --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/requests/lines/main"
l line new feature -q
cd ../feature
printf 'review me\n' > feature.txt
l add feature.txt
l save -qm review
l remote add origin "file://$T/archive" --paper-key-out "$T/store-paper" >/dev/null
l push origin -q
export REQUEST_TEST_ROOT="$T" REQUEST_TEST_BINARY="$LAYR"
python3 - <<'PY'
import json, os, pathlib, subprocess, time, uuid, hashlib
root = pathlib.Path(os.environ['REQUEST_TEST_ROOT'])
binary = os.environ['REQUEST_TEST_BINARY']
base = os.environ.copy()
cwd = pathlib.Path.cwd()
checks = 0

def check(value, label):
    global checks
    assert value, label
    checks += 1
    print('PASS', label, flush=True)

def run(args, env=base, success=True):
    p = subprocess.run([binary, *args], cwd=cwd, env=env, text=True, capture_output=True, timeout=60)
    if success:
        assert p.returncode == 0, (args, p.stderr)
    return p

def data(args, env=base):
    return json.loads(run(args, env).stdout)

def person(login):
    env = dict(base, LAYR_PERSON_DIR=str(root / login), LAYR_PERSON_LOGIN=login)
    paper = root / (login + '.paper')
    run(['person', 'keys', '--paper-key-out', str(paper), '--store', 'file://' + str(root / 'archive')], env)
    env['LAYR_PERSON_KEY_FILE'] = str(paper)
    public = data(['person', 'show', '--json'], env)['signing_key']
    run(['access', 'person-key', login, public])
    return env

ann = person('ann@example.com')
bob = person('bob@example.com')
run(['access', 'grant', 'person:ann@example.com', 'maintainer'])
run(['access', 'grant', 'person:bob@example.com', 'writer'])
run(['protect', 'main', '--approvers', 'person:ann@example.com', '--approvals', '1'])
state = data(['log', '-n', '1', '--json'])['id']
change = next(c['change_id'] for c in data(['change', 'list', '--json']) if c['message'] == 'review')

def submit(kind, *args, env=ann):
    return data(['request', kind, *args, '--person', '--json'], env)['id']

def apply(id, status='applied'):
    rows = data(['requests', 'apply', '--json'])
    row = next(r for r in rows if r['id'] == id)
    check(row['status'] == status, 'request ' + status)
    return row

def records():
    return [json.loads(line)['r'] for line in run(['op', 'export']).stdout.splitlines()]

def outcome(id):
    return next(r for r in records() if r.get('data', {}).get('request_id') == id)

before_keys = (root / 'archive' / 'people' / 'ann@example.com' / 'signing.age').read_bytes()
run(['person', 'keys', '--store', 'file://' + str(root / 'archive')], ann)
check((root / 'archive' / 'people' / 'ann@example.com' / 'signing.age').read_bytes() == before_keys, 'repeated person publication keeps the same encrypted signing identity')
approval = submit('approve', state)
request_path = root / 'archive' / 'requests' / (approval + '.json')
body = json.loads(request_path.read_text())['r']
check(uuid.UUID(approval).version == 7 and body['expires'] - body['created'] == 86400000, 'UUIDv7 id and default 24 hour expiry')
apply(approval)
record = outcome(approval)
check(record['actor']['name'] == 'person:ann@example.com' and record['actor']['uid'] == 4294967295, 'executor records the person rather than root')
check(record['data']['attestation']['signer'] == 'person:ann@example.com' and json.loads(record['data']['request'])['s'], 'approval retains person signature proof')
refused = submit('approve', state, env=bob)
result = apply(refused, 'refused')
check('not an approver' in result['reason'] and outcome(refused)['op'] == 'request.refused', 'non-approver gets a signed refusal with a plain reason')
check(len(data(['attestations', state, '--kind', 'approval', '--json'])) == 1, 'refused approval writes no attestation')

entry = submit('queue-add', state)
apply(entry)
check(any(e['id'] == entry and e['state'] == state for e in data(['queue', 'list', '--json'])), 'queue request keeps exact state and request identity')
comment = submit('comment', change, 'Review this line.', '--version', 'v1', '--path', 'feature.txt', '--line', '1')
apply(comment)
rows = data(['change', 'show', change, '--json'])['comments']
check(len(rows) == 1 and rows[0]['id'] == comment and rows[0]['path'] == 'feature.txt' and rows[0]['line'] == 1 and rows[0]['actor']['name'] == 'person:ann@example.com', 'change show folds a signed inline person comment')
reply = submit('comment', change, 'The line is intentional.', '--reply-to', comment, env=bob)
apply(reply)
check(data(['change', 'show', change, '--json'])['comments'][1]['reply_to'] == comment, 'another reader can reply to the comment')

before = len(records())
rows = data(['requests', 'apply', '--json'])
check(all(row['status'] == 'ignored' for row in rows) and len(records()) == before, 'replayed requests are ignored without new records')
remove = submit('queue-remove', entry)
apply(remove)
check(not any(e['id'] == entry for e in data(['queue', 'list', '--json'])), 'the person can remove their own queued state')

run(['protect', 'release/test', '--approvers', 'person:ann@example.com', '--approvals', '1', '--update', 'maintainer'])
move = submit('ref-move', 'release/test', state, '--expect', 'none')
apply(move)
check(next(r['target'] for r in data(['ref', 'list', '--json']) if r['name'] == 'release/test') == state, 'ref request creates the expected absent ref')
stale = submit('ref-move', 'release/test', state, '--expect', 'none')
result = apply(stale, 'refused')
check('expected value does not match' in result['reason'], 'stale compare-and-set produces a refusal')

withdraw = submit('withdraw', state)
apply(withdraw)
check(outcome(withdraw)['data']['revokes'] == [record['data']['attestation']['attestation_id']], 'withdrawal names only the person approval')
after_withdraw = submit('ref-move', 'release/test', state, '--expect', state)
result = apply(after_withdraw, 'refused')
check('approval' in result['reason'], 'withdrawn person approval no longer satisfies protected ref checks')
# Two people share one executor uid; Bob must never revoke Ann's evidence.
run(['protect', 'main', '--approvers', 'person:bob@example.com', '--approvals', '1'])
bob_withdraw = submit('withdraw', state, env=bob)
apply(bob_withdraw)
check(outcome(bob_withdraw)['data']['revokes'] == [], 'a different person cannot revoke another person evidence')

expiry = int(time.time() * 1000) + 3000
expired = submit('comment', change, 'Expired text must not appear.', '--expires', str(expiry))
time.sleep(max(0, (expiry - time.time() * 1000) / 1000) + .05)
result = apply(expired, 'refused')
check(result['reason'] == 'request has expired', 'expiry refusal is plain and deterministic')
check(not any(c['text'] == 'Expired text must not appear.' for c in data(['change', 'show', change, '--json'])['comments']), 'expired request has no side effect')

# A valid envelope whose signed bytes are changed must fail before any operation.
tampered = submit('comment', change, 'Original text.')
path = root / 'archive' / 'requests' / (tampered + '.json')
path.write_text(path.read_text().replace('Original text.', 'Changed text.'))
result = apply(tampered, 'refused')
check('signature' in result['reason'] and outcome(tampered)['op'] == 'request.refused', 'a changed signature gets a signed refusal')
check('request' not in outcome(tampered)['data'], 'unverified input is not copied into the refusal record')

run(['access', 'deny', 'person:bob@example.com', 'merge'])
denied = submit('queue-add', state, env=bob)
result = apply(denied, 'refused')
check('deny rule' in result['reason'], 'person deny rule overrides executor permissions')

bad_claim = submit('comment', change, 'Invalid claim must not apply.')
body = json.loads((root / 'archive' / 'requests' / (bad_claim + '.json')).read_text())['r']
(root / 'archive' / 'requests' / 'claims' / (bad_claim + '.json')).write_text(json.dumps({'r': {'format': 1, 'project': body['project'], 'request': bad_claim, 'machine': record['machine']}, 's': 'invalid-signature'}))
result = apply(bad_claim, 'refused')
check(result['reason'] == 'request claim signature does not verify', 'a forged claim by a trusted machine gets a signed refusal')

untrusted = submit('comment', change, 'Untrusted claim must not apply.')
body = json.loads((root / 'archive' / 'requests' / (untrusted + '.json')).read_text())['r']
claim_path = root / 'archive' / 'requests' / 'claims' / (untrusted + '.json')
claim_path.write_text(json.dumps({'r': {'format': 1, 'project': body['project'], 'request': untrusted, 'machine': '01a12290-0000-7000-8000-000000000099'}, 's': 'invalid-untrusted-signature'}))
result = apply(untrusted, 'refused')
check(result['reason'] == 'claimed by an untrusted machine' and outcome(untrusted)['op'] == 'request.refused', 'untrusted claim gets a signed terminal refusal')
check(not any(c['text'] == 'Untrusted claim must not apply.' for c in data(['change', 'show', change, '--json'])['comments']), 'untrusted claim never applies its request')
apply(untrusted, 'ignored')

# Claims survive local retries and prevent a different machine from applying again.
check((root / 'archive' / 'requests' / 'claims' / (entry + '.json')).exists(), 'executor claims each request with an immutable object')
claim = json.loads((root / 'archive' / 'requests' / 'claims' / (entry + '.json')).read_text())
check(claim['r']['request'] == entry and claim['s'], 'claim binds machine, project and request with a signature')

replica = dict(base, LAYR_ROOT=str(root / 'store' / 'replica-root'), LAYR_PAPER_KEY_FILE=str(root / 'store-paper'))
run(['clone', 'file://' + str(root / 'archive'), 'replica', '-q'], replica)
other = data(['--project', 'replica', 'requests', 'apply', '--json'], replica)
check(other and all(row['status'] == 'claimed' or (row['id'] in (untrusted, bad_claim) and row['status'] == 'refused') for row in other), 'a second trusted machine cannot apply claimed requests')
check(not data(['--project', 'replica', 'queue', 'list', '--json'], replica), 'second machine creates no duplicate queue effects')

run(['reindex', '-q'])
before = len(records())
rows = data(['requests', 'apply', '--json'])
check(all(r['status'] == 'ignored' for r in rows) and len(records()) == before, 'reindex preserves request idempotency')
check('signatures ok' in run(['fsck']).stdout, 'all applied operations and refusals verify after reindex')
check(len(data(['change', 'show', change, '--json'])['comments']) == 2, 'reindex preserves comments and replies')
for login in ['ann@example.com', 'bob@example.com']:
    published = root / 'archive' / 'people' / login
    public = json.loads((published / 'person.json').read_text())
    check(public['signingKey'] and (published / 'signing.age').read_bytes().startswith(b'age-encryption.org/'), 'published person signing identity is encrypted')
print(f'request checks: {checks}', flush=True)
PY
finish
