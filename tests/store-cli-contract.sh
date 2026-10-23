#!/bin/bash
# Run after remote-store integration. REMOTE_URL can select a fresh S3 test prefix.
# Keep all paper-key and credential values inside this process, including on failure.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/src"
l init key-contract --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/key-contract/lines/main"
url=${REMOTE_URL:-file://$T/archive}
case "$url" in
    file://*)
        export AWS_ACCESS_KEY_ID=${AWS_ACCESS_KEY_ID:-layr-cli-contract-access}
        export AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY:-layr-cli-contract-secret}
        export AWS_SESSION_TOKEN=${AWS_SESSION_TOKEN:-layr-cli-contract-session}
        ;;
esac
export AWS_REGION=${AWS_REGION:-us-east-1}

# A failed creation must not leave a configured remote.
rc=0
l remote add archive "$url" > "$T/no-key.out" 2> "$T/no-key.err" || rc=$?
eq "a new store requires a paper-key output path" 128 "$rc"
has "the error names the output flag" --paper-key-out "$(cat "$T/no-key.err")"
no "a failed creation does not record the URL" l config --get cloud.remote.archive.url

printf 'keep\n' > "$T/existing"
existing_output() { l remote add archive "$url" --paper-key-out "$T/existing" > "$T/existing.out" 2> "$T/existing.err"; }
no "the output path must not exist" existing_output
eq "an existing output file stays unchanged" keep "$(cat "$T/existing")"
line_output() { l remote add archive "$url" --paper-key-out "$PWD/paper-key" > "$T/line.out" 2> "$T/line.err"; }
no "the output file cannot be inside a line" line_output
no "a refused line path leaves no key file" test -e "$PWD/paper-key"
if [ -z "${REMOTE_URL:-}" ]; then
    inside_store_output() { l remote add archive "$url" --paper-key-out "$T/archive/paper-key" > "$T/store.out" 2> "$T/store.err"; }
    no "the output file cannot be inside the remote store" inside_store_output
    no "a refused store path leaves no key file" test -e "$T/archive/paper-key"
fi

add_store() { l remote add archive "$url" --paper-key-out "$T/paper-key" --json > "$T/add.out" 2> "$T/add.err"; }
ok "remote add creates the store and the paper-key file" add_store
if [ ! -f "$T/paper-key" ]; then finish; exit 1; fi
eq "the paper-key file has mode 0600" 600 "$(stat -c %a "$T/paper-key")"
eq "the paper-key file belongs to the caller" "$(id -u)" "$(stat -c %u "$T/paper-key")"
l op export > "$T/op-log"
l config --list > "$T/config"
ok "the key is absent from output, records, configuration and database files" python3 - "$T" "$LAYR_ROOT/projects/key-contract" <<'PY'
import pathlib, sys
root, project = map(pathlib.Path, sys.argv[1:])
key = (root / 'paper-key').read_bytes().strip()
assert key
for name in ['add.out', 'add.err', 'no-key.out', 'no-key.err', 'existing.out', 'existing.err', 'line.out', 'line.err', 'op-log', 'config']:
    assert key not in (root / name).read_bytes()
for path in (project / 'meta/db').iterdir():
    if path.is_file(): assert key not in path.read_bytes()
PY
ok "JSON returns only the paper-key path" python3 - "$T/add.out" "$T/paper-key" <<'PY'
import json,sys
value=json.load(open(sys.argv[1]))
assert value['recovery_key_file']==sys.argv[2]
assert set(value)=={'name','url','recovery_key_file'}
PY

push_store() { l push archive --json > "$T/push.out" 2> "$T/push.err"; }
ok "push uses the caller environment" push_store
l op export > "$T/op-log"
l config --list > "$T/config"
ok "push stores no caller credentials and prints no paper key" python3 - "$T" "$LAYR_ROOT/projects/key-contract" <<'PY'
import os,pathlib,sys
root,project=map(pathlib.Path,sys.argv[1:])
secrets=[os.environ[k].encode() for k in ['AWS_ACCESS_KEY_ID','AWS_SECRET_ACCESS_KEY','AWS_SESSION_TOKEN'] if os.environ.get(k)]
secrets.append((root/'paper-key').read_bytes().strip())
paths=[root/name for name in ['add.out','add.err','push.out','push.err','op-log','config']]
paths.extend(p for p in (project/'meta').rglob('*') if p.is_file())
for path in paths:
    value=path.read_bytes()
    assert all(secret not in value for secret in secrets)
PY
finish
