#!/bin/bash
# Automatic exact-version checks, isolated execution, and signed retained logs.
. "$(dirname "$0")/lib.sh"
set -e
state_id() { l log "$1" -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }
id layr-check-reader >/dev/null 2>&1 || useradd -m layr-check-reader
mkdir "$T/source"
echo initial > "$T/source/value"
l init p --from "$T/source" -q >/dev/null
AR=$LAYR_ROOT
A=$AR/projects/p/lines/main
cd "$A"
l access grant layr-check-reader reader
l config checks.changes '["smoke","fail","root"]'
l config checks.lines '{"main":["smoke"]}'
l config checks.privileged '["root"]'
l config checks.timeout 5
l config check.smoke 'python3 -c '\''import json,os,resource,socket; assert os.getuid()!=0; assert "CHECK_SECRET" not in os.environ; assert not os.path.exists("/root/.ssh"); assert not os.path.exists("/etc/shadow"); assert not os.path.exists("/home/layr-check-reader/.ssh"); assert not os.path.exists("/run/layr"); assert resource.getrlimit(resource.RLIMIT_AS)[1]==4294967296; assert resource.getrlimit(resource.RLIMIT_NPROC)[1]==256; assert not any(line.split()[1]=="00000000" for line in open("/proc/net/route").readlines()[1:]); print(json.dumps({"uid":os.getuid(),"state":os.environ["LAYR_TRY_STATE"],"value":open("value").read().strip()}))'\'''
l config check.fail 'echo failed-check; exit 7'
l config check.root 'test "$(id -u)" = 0 && test -z "${CHECK_SECRET:-}" && printf "root:%s\n" "$PWD"'
SMOKE=$(l config check.smoke)
l remote add archive "file://$T/archive" --paper-key-out "$T/key" >/dev/null
l push archive -q
# A separate machine receives the source/config before its watcher runs.
new_fs "$T/executor" 4G
export LAYR_ROOT=$T/executor/store LAYR_PAPER_KEY_FILE=$T/key
cd "$T"
l clone "file://$T/archive" p -q >/dev/null
unset LAYR_PAPER_KEY_FILE
BR=$LAYR_ROOT
B=$BR/projects/p/lines/main
# Only the creator defines automatic check policy. The runner's local settings cannot replace it.
cd "$B"
l access grant layr-check-reader reader
l protect main --no-direct
l config checks.changes '["smoke","fail","root"]'
l config checks.lines '{"main":["smoke"]}'
l config checks.privileged '["root"]'
l config checks.timeout 5
l config check.smoke "$SMOKE"
l config check.fail 'echo failed-check; exit 7'
l config check.root 'test "$(id -u)" = 0 && test -z "${CHECK_SECRET:-}" && printf "root:%s\n" "$PWD"'
chmod 755 "$T"
cp "$LAYR" "$T/layr"
export LAYR_SOCKET=$T/layr.sock
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
stop_watchers() { pkill -P "$DPID" 2>/dev/null || true; }
trap 'stop_watchers; kill ${DPID:-} ${WPID:-} 2>/dev/null || true; cleanup' EXIT
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep .1; done
as_reader() { (cd / && runuser -u layr-check-reader -- env LAYR_SOCKET="$LAYR_SOCKET" CHECK_SECRET=never-log-this-secret "$T/layr" --project p "$@"); }
watch_checks() { as_reader checks watch "$@"; }
project_config() {
    (export LAYR_ROOT=$AR; cd "$A"; l config "$@"; l push archive -q)
    l fetch origin -q
}
cd "$B"
no 'root cannot invoke the watcher' l checks watch --once
no 'zero polling interval is rejected' watch_checks --once --interval 0
no 'project config cannot enable privileged execution' l config checks.privileged-runner true
no 'project config cannot enable a runner' l config checks.runner true
no 'local execution opt-in requires root' as_reader config --local checks.privileged-runner true
no 'a reader cannot opt this machine in as a runner' as_reader config --local checks.runner true
no 'invalid check config is rejected' l config checks.changes smoke
no 'zero timeout is rejected' l config checks.timeout 0
no 'privileged-only requires local opt-in' watch_checks --once --privileged-only
report=$(watch_checks --once --json)
ok 'a machine without runner opt-in only reports all pending checks' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert not r["work_done"] and r["checks"]==[] and r["pending"]>r["pending_privileged"]>0' "$report"
l config --local checks.privileged-runner true
ok 'privileged opt-in alone never enables execution' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert not r["work_done"] and r["pending"]>0' "$(watch_checks --once --json)"
l config --local checks.privileged-runner false
ok 'root opts this machine in as a runner' l config --local checks.runner true
# Initial main is checked even when it has no logical change.
rc=0; first=$(watch_checks --once --json) || rc=$?
ok 'the initial main gets a sandboxed check' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["work_done"] and any(c["name"]=="smoke" and c["exit"]==0 for c in r["checks"]); assert all(not c["privileged"] for c in r["checks"])' "$first"
idle=$(watch_checks --once --json)
ok 'rerun is idle with pending privileged work' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert not r["work_done"] and r["count"]==0 and not r["checks"] and r["pending_privileged"]>0' "$idle"
no 'a reader cannot change command policy' as_reader config check.smoke true
no 'a reader cannot change watcher policy' as_reader config checks.changes '[]'
# A new pushed change version arrives while the watcher remains active.
watch_checks --json --interval 1 > "$T/watch.jsonl" 2> "$T/watch.err" &
WPID=$!
export LAYR_ROOT=$AR
cd "$A"
l line new topic main -q
cd ../topic
echo pushed > value
l save -qam pushed
l approve head -q
PUSHED=$(state_id head)
l push archive -q
export LAYR_ROOT=$BR
cd "$B"
l fetch origin -q
for i in $(seq 1 100); do
    if python3 - "$T/watch.jsonl" "$PUSHED" 2>/dev/null <<'PY'
import json,sys
rows=[]
for line in open(sys.argv[1]):
    try: rows+=json.loads(line)['checks']
    except (ValueError,KeyError): pass
assert any(r['state']==sys.argv[2] and r['name']=='fail' and r['exit']==7 for r in rows)
PY
    then break; fi
    sleep .1
done
ok 'a pushed version automatically records success and failure' python3 - "$T/watch.jsonl" "$PUSHED" <<'PY'
import json,sys
rows=[c for line in open(sys.argv[1]) for c in json.loads(line)['checks'] if c['state']==sys.argv[2]]
assert len(rows)==2 and {c['exit'] for c in rows}=={0,7}
PY
export LAYR_ROOT=$AR
cd "$A/../topic"
echo amended > value
l save -qam amended --amend
l approve head -q
AMENDED=$(state_id head)
l push archive -q
export LAYR_ROOT=$BR
cd "$B"
l fetch origin -q
for i in $(seq 1 100); do
    if python3 - "$T/watch.jsonl" "$AMENDED" 2>/dev/null <<'PY'
import json,sys
rows=[]
for line in open(sys.argv[1]):
    try: rows+=json.loads(line)['checks']
    except (ValueError,KeyError): pass
assert any(r['state']==sys.argv[2] and r['name']=='fail' for r in rows)
PY
    then break; fi
    sleep .1
done
ok 'an amended version gets its own exact-state evidence' python3 - "$T/watch.jsonl" "$PUSHED" "$AMENDED" <<'PY'
import json,sys
rows=[c for line in open(sys.argv[1]) for c in json.loads(line)['checks']]
for state in sys.argv[2:]:
 assert len([c for c in rows if c['state']==state])==2
PY
no 'a concurrent watcher cannot acquire the project lock' watch_checks --once
stop_watchers
wait "$WPID" 2>/dev/null || true
unset WPID
sleep .1
idle=$(watch_checks --once --json)
ok 'failed checks are also deduplicated' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert not r["work_done"] and r["checks"]==[]' "$idle"
DB=$BR/projects/p/meta/db/layr.sqlite
ok 'exact argv, version and retained log attestations are signed' python3 - "$DB" "$PUSHED" "$BR/projects/p/states" <<'PY'
import json,pathlib,sqlite3,sys
d=sqlite3.connect(sys.argv[1]); rows=[json.loads(r[0])['r'] for r in d.execute("select raw from records where op='attest'")]
rows=[r for r in rows if r['data']['attestation']['subject']['id']==sys.argv[2] and r['data']['attestation']['kind']=='check']
assert len(rows)==2
for r in rows:
 c=r['data']['check']; a=r['data']['attestation']; assert c['argv']==['sh','-c',c['command']] and a['kind']=='check'
 log=(pathlib.Path(sys.argv[3])/c['output_state']/'check.log').read_text()
 assert 'never-log-this-secret' not in log
 if c['name']=='smoke': assert json.loads(log)['value']=='pushed'
 else: assert log.strip()=='failed-check'
 assert d.execute('select target from refs where name=?',(c['log_ref'],)).fetchone()[0]==c['output_state']
PY
ok 'signed check and derived output records verify' l fsck
as_reader attest revoke "$PUSHED" >/dev/null
rc=0; retried=$(watch_checks --once --json) || rc=$?
eq 'a revoked failed check reruns and reports failure' 1 "$rc"
ok 'revocation permits exactly the revoked version to rerun' python3 -c 'import json,sys; rows=json.loads(sys.argv[1])["checks"]; assert len(rows)==2 and all(r["state"]==sys.argv[2] for r in rows)' "$retried" "$PUSHED"
# Multiple protected main heads arriving together must all be checked.
project_config checks.changes '[]'
project_config check.smoke 'echo checked-main'
export LAYR_ROOT=$AR
cd "$A"
echo main-one > value
l save -qam main-one
MAIN1=$(state_id head)
echo main-two > value
l save -qam main-two
MAIN2=$(state_id head)
l push archive -q
export LAYR_ROOT=$BR
cd "$B"
l fetch origin -q
main_results=$(watch_checks --once --json)
ok 'both intermediate main heads run after one fetch' python3 -c 'import json,sys; rows=json.loads(sys.argv[1])["checks"]; assert all(any(r["state"]==s and r["exit"]==0 for r in rows) for s in sys.argv[2:])' "$main_results" "$MAIN1" "$MAIN2"
# Exact argv changes cause a rerun; output is bounded even if the child keeps writing.
project_config checks.changes '[]'
project_config checks.lines '{"main":["smoke"]}'
project_config checks.log-bytes 128
project_config check.smoke 'python3 -c '\''print("x"*10000)'\'''
bounded=$(watch_checks --once --json)
ok 'a changed exact argv runs again and caps the log' python3 - "$bounded" "$BR/projects/p/states" <<'PY'
import json,pathlib,sys
rows=json.loads(sys.argv[1])['checks']; assert rows
for r in rows:
 assert r['truncated'] and len((pathlib.Path(sys.argv[2])/r['output_state']/'check.log').read_bytes())==128
PY
project_config check.smoke 'sleep 30 & wait'
project_config checks.timeout 1
rc=0; timed=$(watch_checks --once --json) || rc=$?
eq 'a timed out scan exits one' 1 "$rc"
ok 'timeout records failure with a log reference' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["work_done"] and all(c["exit"]==124 and c["log_ref"] for c in r["checks"])' "$timed"
# Privileged checks require both independent configuration switches.
project_config checks.timeout 5
project_config checks.changes '["root"]'
project_config checks.lines '{}'
ok 'root opts this machine in locally' l config --local checks.privileged-runner true
eq 'local opt-in is readable' true "$(l config --local --get checks.privileged-runner)"
no 'local opt-in is absent from project config' l config --get checks.privileged-runner
(export LAYR_ROOT=$AR; cd "$A/../topic"; echo unapproved > value; l save -qam unapproved; l push archive -q)
l fetch origin -q
priv=$(watch_checks --once --privileged-only --json)
ok 'unapproved change versions remain pending on a privileged runner' python3 -c 'import json,sys; assert json.loads(sys.argv[1])["pending_privileged"]==1' "$priv"
ok 'privileged checks record root execution in a fresh removed folder' python3 - "$priv" "$BR/projects/p/states" <<'PY'
import json,pathlib,sys
rows=json.loads(sys.argv[1])['checks']; assert rows
for r in rows:
 assert r['privileged'] and r['exit']==0
 log=(pathlib.Path(sys.argv[2])/r['output_state']/'check.log').read_text().strip()
 assert log.startswith('root:/var/tmp/layr-check-') and not pathlib.Path(log[5:]).exists()
PY
(export LAYR_ROOT=$AR; cd "$A/../topic"; l approve head -q; l push archive -q)
l fetch origin -q
approved=$(watch_checks --once --privileged-only --json)
ok 'a creator maintainer approval releases the exact privileged version' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["count"]==1 and r["checks"][0]["exit"]==0 and r["pending_privileged"]==0' "$approved"
ok 'privileged reruns are deduplicated' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert not r["work_done"] and r["count"]==0 and r["pending_privileged"]==0' "$(watch_checks --once --privileged-only --json)"
project_config check.root 'set -e; mkdir m; mount -t tmpfs tmpfs m; printf "%s\n" "$PWD"; sleep 30'
project_config checks.timeout 1
rc=0; mounted=$(watch_checks --once --privileged-only --json) || rc=$?
eq 'privileged timeout reports a failed scan' 1 "$rc"
ok 'timed out root checks release their mounts and work folders' python3 - "$mounted" "$BR/projects/p/states" <<'PY'
import json,pathlib,sys
rows=json.loads(sys.argv[1])['checks']; assert rows
for r in rows:
 assert r['privileged'] and r['exit']==124
 path=(pathlib.Path(sys.argv[2])/r['output_state']/'check.log').read_text().strip()
 assert path.startswith('/var/tmp/layr-check-') and not pathlib.Path(path).exists()
PY
project_config checks.timeout 5
ok 'local config is not a signed project record' python3 -c 'import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); assert not d.execute("select 1 from config where key=?",("checks.privileged-runner",)).fetchone()' "$DB"
l config --local checks.privileged-runner false
ok 'completed privileged results suppress another wakeup' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["pending_privileged"]==0' "$(watch_checks --once --json)"
no 'disabled privileged-only watcher fails loudly' watch_checks --once --privileged-only
# Send the signed runner evidence back to the original machine.
read -r runner_machine runner_key < <(l remote list --json | python3 -c 'import json,sys; m=next(m for m in json.load(sys.stdin)["machines"] if m["current"]); print(m["id"],m["key"])')
export LAYR_ROOT=$AR
cd "$A"
l remote trust archive "$runner_machine" --public-key "$runner_key" >/dev/null
export LAYR_ROOT=$BR
cd "$B"
l push origin -q
stop_watchers
kill "$DPID"
wait "$DPID" 2>/dev/null || true
export LAYR_ROOT=$AR LAYR_SOCKET=$T/source.sock
cd "$A"
l fetch archive -q
l config checks.changes '["root"]'
l config checks.lines '{"main":["smoke"]}'
l config check.smoke 'sleep 30 & wait'
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/source-daemon.log" 2>&1 &
DPID=$!
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep .1; done
returned=$(watch_checks --once --json)
ok 'synced runner attestations suppress wakeups on another machine' python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["pending"]==r["pending_privileged"]==0 and not r["work_done"]' "$returned"
eq 'runner opt-in does not sync to the reporter' false "$(as_reader config --local --get checks.runner)"
eq 'privileged opt-in does not sync to the reporter' false "$(as_reader config --local --get checks.privileged-runner)"
ok 'returned log refs are readable through the normal show command' l show "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["checks"][0]["log_ref"])' "$priv"):check.log"
finish
