#!/bin/bash
# ci: macos overlayfs
# The merge machine must use the creator's approval policy, even if its local rule differs.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init p --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l remote add origin "file://$T/archive" --paper-key-out "$T/store-paper" >/dev/null
l config check.gate true >/dev/null
l protect main --check gate --approvals 1 >/dev/null
l line new -q feature
cd ../feature
echo review > review.txt; l add review.txt; l save -qm review
export REQUEST_TEST_ROOT="$T" REQUEST_TEST_BINARY="$LAYR"
python3 - <<'PY'
import json, os, pathlib, subprocess
root = pathlib.Path(os.environ['REQUEST_TEST_ROOT'])
binary = os.environ['REQUEST_TEST_BINARY']
owner_env = os.environ.copy()
other_env = dict(owner_env, LAYR_ROOT=str(root / 'other' / 'store'))
cwd = pathlib.Path.cwd()
count = 0

def run(args, env=owner_env, success=True):
    result = subprocess.run([binary, *args], cwd=cwd, env=env, text=True, capture_output=True, timeout=60)
    if success:
        assert result.returncode == 0, (args, result.stderr)
    return result

def data(args, env=owner_env):
    return json.loads(run(args, env).stdout)

def check(value, label):
    global count
    assert value, label
    count += 1
    print('PASS', label, flush=True)

state = run(['rev-parse', 'head']).stdout.strip()
head = run(['rev-parse', 'main']).stdout.strip()
entry = data(['queue', 'add', state, '--json'])['id']
other = data(['remote', 'self', '--json'], other_env)
run(['remote', 'trust', 'origin', other['id'], '--public-key', other['public_key']])
run(['push', 'origin'])
run(['clone', 'file://' + str(root / 'archive'), 'p', '--heads'], other_env)
run(['--project', 'p', 'config', 'check.gate', 'true'], other_env)
run(['--project', 'p', 'protect', 'main', '--check', 'gate'], other_env)

def candidate(entry_id=entry, source=state):
    return run(['--project', 'p', 'queue', 'candidate', entry_id, '--expect-head', head,
                '--expect-state', source, '--json'], other_env, success=False)

run(['config', 'check.extra', 'true'])
run(['protect', 'main', '--check', 'gate,extra', '--approvals', '1'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'creator check policy differs' in result.stderr,
      'a creator-required check missing from the merge rule blocks the candidate')
run(['protect', 'main', '--check', 'gate', '--approvals', '1'])
run(['config', 'check.gate', 'false'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'creator check policy differs' in result.stderr,
      'a different creator check command blocks the candidate')
run(['config', 'check.gate', 'true'])
run(['config', 'checks.platforms', '{"gate":"linux"}'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'creator check policy differs' in result.stderr,
      'a different creator check platform blocks the candidate')
run(['config', 'checks.platforms', '{}'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'creator approval required' in result.stderr, 'creator rule blocks an unapproved state')
run(['--project', 'p', 'approve', '-q', state], other_env)
result = candidate()
check(result.returncode != 0 and 'creator approval required' in result.stderr, 'another machine uid cannot satisfy creator approval')
person_env = dict(owner_env, LAYR_PERSON_DIR=str(root / 'reviewer'), LAYR_PERSON_LOGIN='ann@example.com')
paper = root / 'reviewer.paper'
run(['person', 'keys', '--paper-key-out', str(paper), '--store', 'file://' + str(root / 'archive')], person_env)
person_env['LAYR_PERSON_KEY_FILE'] = str(paper)
public = data(['person', 'show', '--json'], person_env)['signing_key']
run(['access', 'person-key', 'ann@example.com', public])
run(['access', 'grant', 'person:ann@example.com', 'maintainer'])

def request(action, env=person_env):
    request_id = data(['request', action, state, '--person', '--json'], env)['id']
    rows = data(['requests', 'apply', '--no-run-queue', '--json'])
    assert any(row['id'] == request_id and row['status'] == 'applied' for row in rows), rows
    run(['push', 'origin'])
    run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)

request('approve')
result = candidate()
check(result.returncode == 0 and json.loads(result.stdout)['candidate'], 'creator-pinned person approval permits candidate preparation')
run(['protect', 'main', '--check', 'gate,extra', '--approvals', '1'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'creator check policy differs' in result.stderr,
      'an approved change still waits for every creator-required check')
run(['protect', 'main', '--check', 'gate', '--approvals', '1'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
run(['protect', 'main', '--keep-approvals', 'docs'])
(cwd / 'docs').mkdir()
(cwd / 'docs/note.txt').write_text('documentation\n')
run(['add', 'docs/note.txt'])
run(['save', '-qm', 'docs', '--amend'])
docs_state = run(['rev-parse', 'head']).stdout.strip()
docs_entry = data(['queue', 'add', docs_state, '--json'])['id']
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate(docs_entry, docs_state)
check(result.returncode == 0, 'creator policy carries approval over an allowed docs-only amend')
run(['queue', 'remove', docs_entry])
(cwd / 'review.txt').write_text('changed code\n')
run(['save', '-qam', 'code', '--amend'])
code_state = run(['rev-parse', 'head']).stdout.strip()
code_entry = data(['queue', 'add', code_state, '--json'])['id']
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate(code_entry, code_state)
check(result.returncode != 0 and 'creator approval required' in result.stderr, 'a code amend invalidates the earlier approval')
run(['queue', 'remove', code_entry])
request('withdraw')
result = candidate()
check(result.returncode != 0 and 'creator approval required' in result.stderr, 'withdrawing the approval blocks the same entry')
request('approve')
bob_env = dict(owner_env, LAYR_PERSON_DIR=str(root / 'bob'), LAYR_PERSON_LOGIN='bob@example.com')
bob_paper = root / 'bob.paper'
run(['person', 'keys', '--paper-key-out', str(bob_paper), '--store', 'file://' + str(root / 'archive')], bob_env)
bob_env['LAYR_PERSON_KEY_FILE'] = str(bob_paper)
bob_public = data(['person', 'show', '--json'], bob_env)['signing_key']
run(['access', 'person-key', 'bob@example.com', bob_public])
run(['access', 'grant', 'person:bob@example.com', 'writer'])
run(['protect', 'main', '--owner', 'review.txt=person:bob@example.com'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'missing owner approval' in result.stderr, 'creator path owner approval is required')
request('approve', bob_env)
result = candidate()
check(result.returncode == 0, 'creator path owner approval permits candidate preparation')
request('withdraw', bob_env)
result = candidate()
check(result.returncode != 0 and 'missing owner approval' in result.stderr, 'withdrawing owner approval blocks the entry')
request('approve', bob_env)
run(['access', 'grant', 'person:ann@example.com', 'writer'])
run(['push', 'origin'])
run(['--project', 'p', 'fetch', 'origin', '--heads'], other_env)
result = candidate()
check(result.returncode != 0 and 'not enough approvals' in result.stderr, 'a revoked creator-side approver no longer counts')
check(run(['--project', 'p', 'rev-parse', 'main'], other_env).stdout.strip() == head, 'approval checks do not move main')
(root / 'request-counts').write_text(str(count))
PY
PASS=$((PASS + $(cat "$T/request-counts")))
finish
