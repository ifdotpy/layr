#!/bin/bash
# ci: macos overlayfs
# Path authorization uses the signed requester's current policy, not the queue runner's uid.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
echo base > "$T/source/secret.txt"
l init p --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l line new -q feature
cd ../feature
echo changed > secret.txt
l save -qam change
l remote add origin "file://$T/archive" --paper-key-out "$T/store-paper" >/dev/null
l push origin -q >/dev/null
export REQUEST_TEST_ROOT="$T" REQUEST_TEST_BINARY="$LAYR"
python3 - <<'PY'
import json, os, pathlib, subprocess
root = pathlib.Path(os.environ['REQUEST_TEST_ROOT'])
binary = os.environ['REQUEST_TEST_BINARY']
base = os.environ.copy()
cwd = pathlib.Path.cwd()
count = 0

def run(args, env=base, success=True):
    result = subprocess.run([binary, *args], cwd=cwd, env=env, text=True, capture_output=True, timeout=60)
    if success:
        assert result.returncode == 0, (args, result.stderr)
    return result

def data(args, env=base):
    return json.loads(run(args, env).stdout)

def check(value, label):
    global count
    assert value, label
    count += 1
    print('PASS', label, flush=True)

def person(login, role):
    env = dict(base, LAYR_PERSON_DIR=str(root / login), LAYR_PERSON_LOGIN=login)
    paper = root / (login + '.paper')
    run(['person', 'keys', '--paper-key-out', str(paper), '--store', 'file://' + str(root / 'archive')], env)
    env['LAYR_PERSON_KEY_FILE'] = str(paper)
    public = data(['person', 'show', '--json'], env)['signing_key']
    run(['access', 'person-key', login, public])
    run(['access', 'grant', 'person:' + login, role])
    return env

admin = person('ann@example.com', 'admin')
writer = person('bob@example.com', 'writer')
run(['protect', 'main', '--path', 'secret.txt=admin'])
run(['config', 'queue.run', 'main'])
state = run(['rev-parse', 'head']).stdout.strip()
head = run(['rev-parse', 'main']).stdout.strip()

def submit(env):
    request = data(['request', 'queue-add', state, '--person', '--json'], env)['id']
    applied = data(['requests', 'apply', '--no-run-queue', '--json'])
    assert any(row['id'] == request and row['status'] == 'applied' for row in applied), applied
    check(run(['rev-parse', 'main']).stdout.strip() == head, 'applying the request does not run the configured queue')
    return request

def candidate(entry):
    return run(['queue', 'candidate', entry, '--expect-head', head, '--expect-state', state, '--json'], success=False)

bob = submit(writer)
result = candidate(bob)
check(result.returncode != 0 and 'lacks verified rights' in result.stderr, 'writer cannot queue an admin path: ' + result.stderr + result.stdout)
ann = submit(admin)
result = candidate(ann)
check(result.returncode == 0 and json.loads(result.stdout)['candidate'], 'signed admin can queue an admin path')
run(['access', 'deny', 'person:ann@example.com', 'merge'])
result = candidate(ann)
check(result.returncode != 0 and 'lacks verified rights' in result.stderr, 'a revoked merge right stops the pending entry')
check(run(['rev-parse', 'main']).stdout.strip() == head, 'path checks never moved main')
(root / 'request-counts').write_text(str(count))
PY
PASS=$((PASS + $(cat "$T/request-counts")))
finish
