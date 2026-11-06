#!/bin/bash
# ci: macos
# Path rules of approvals (docs/review.md). `--keep-approvals <paths>`: an approval of an earlier version of a change
# counts for a newer version that changed only those paths after it. `--owner <path>=<principal>`: a change of the
# path needs an approval by its owner. People approve through signed requests, as in t41.
. "$(dirname "$0")/lib.sh"
# Root takes a protected line without approvals.
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
set -e
sample "$T/source"
l init owners --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/owners/lines/main"
mkdir -p docs secret src
echo doc > docs/a.md; echo key > secret/k.txt; echo code > src/x.rs
l add docs/a.md secret/k.txt src/x.rs >/dev/null
l save -qm base
l line new feature -q
l remote add origin "file://$T/archive" --paper-key-out "$T/store-paper" >/dev/null
l push origin -q
export REQUEST_TEST_ROOT="$T" REQUEST_TEST_BINARY="$LAYR" REQUEST_TEST_PROJECT="$LAYR_ROOT/projects/owners/lines"
python3 - <<'PY'
import json, os, pathlib, subprocess
root = pathlib.Path(os.environ['REQUEST_TEST_ROOT'])
binary = os.environ['REQUEST_TEST_BINARY']
lines = pathlib.Path(os.environ['REQUEST_TEST_PROJECT'])
base = os.environ.copy()
main, feature = lines / 'main', lines / 'feature'
checks = 0

def check(value, label):
    global checks
    assert value, label
    checks += 1
    print('PASS', label, flush=True)

def run(args, cwd=main, env=base, success=True):
    p = subprocess.run([binary, *args], cwd=cwd, env=env, text=True, capture_output=True, timeout=120)
    if success:
        assert p.returncode == 0, (args, p.stderr)
    return p

def data(args, cwd=main, env=base):
    return json.loads(run(args, cwd, env).stdout)

def person(login):
    env = dict(base, LAYR_PERSON_DIR=str(root / login), LAYR_PERSON_LOGIN=login)
    paper = root / (login + '.paper')
    run(['person', 'keys', '--paper-key-out', str(paper), '--store', 'file://' + str(root / 'archive')], env=env)
    env['LAYR_PERSON_KEY_FILE'] = str(paper)
    run(['access', 'person-key', login, data(['person', 'show', '--json'], env=env)['signing_key']])
    return env

ann = person('ann@example.com')
bob = person('bob@example.com')
run(['access', 'grant', 'person:ann@example.com', 'maintainer'])
run(['access', 'grant', 'person:bob@example.com', 'writer'])

def approve(state, env):
    id = data(['request', 'approve', state, '--into', 'main', '--person', '--json'], env=env)['id']
    row = next(r for r in data(['requests', 'apply', '--json']) if r['id'] == id)
    check(row['status'] == 'applied', 'the approval request is applied')

def save(path, text, message='change', line=None):
    line = line or feature
    (line / path).write_text(text)
    run(['add', path], cwd=line)
    run(['save', '-qm', message, '--amend'] if message == 'amend' else ['save', '-qm', message], cwd=line)
    return data(['log', '-n', '1', '--json'], cwd=line)['id']

def merge(state):
    p = run(['merge', state, '--expect', state, '--no-ff', '-m', 'merge'], success=False)
    p.stderr = p.stdout + p.stderr
    return p

# Carry an approval over a docs-only amend, not over a code amend.
run(['protect', 'main', '--approvers', 'person:ann@example.com', '--approvals', '1', '--keep-approvals', 'docs'])
v1 = save('src/x.rs', 'code v1\n')
approve(v1, ann)
v2 = save('docs/a.md', 'doc v2\n', 'amend')
check(v2 != v1, 'the amend makes a new version')
out = run(['protect'], success=True).stdout
check('keep-approvals docs' in out, 'the rule lists the paths that keep approvals')
p = merge(v2)
check(p.returncode == 0, 'a version that changed only docs after the approval merges: ' + p.stderr)
v3 = save('src/x.rs', 'code v3\n', 'amend')
p = merge(v3)
check(p.returncode != 0 and 'approval' in p.stderr, 'a version that changed code after the approval needs a new one')
run(['protect', 'main', '--keep-approvals', ''])
v4 = save('docs/a.md', 'doc v4\n', 'amend')
approve(v3, ann)
p = merge(v4)
check(p.returncode != 0, 'without --keep-approvals an approval holds only its exact state')

# An owner of secret/: a change of it needs bob's approval as well. A new line from main.
run(['protect', 'main', '--owner', 'secret=person:bob@example.com'])
check('owner secret=person:bob@example.com' in run(['protect']).stdout, 'the rule lists the owner')
run(['line', 'new', 'owned', 'main', '-q'])
v5 = save('secret/k.txt', 'key v5\n', 'secret', line=lines / 'owned')
approve(v5, ann)
p = merge(v5)
check(p.returncode != 0 and "changes 'secret'" in p.stderr and 'bob@example.com' in p.stderr, 'a change of an owned path needs its owner: ' + p.stderr)
approve(v5, bob)
p = merge(v5)
check(p.returncode == 0, 'with the owner approval the merge goes on: ' + p.stderr)
check(run(['protect', 'env/prod', '--owner', 'a=user:1'], success=False).returncode != 0, 'ref protection refuses --owner')
(root / 'request-counts').write_text(str(checks))
PY
PASS=$((PASS + $(cat "$T/request-counts")))
finish
