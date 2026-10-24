#!/bin/bash
# Real S3 calls use AWS_PROFILE keys without AWS key environment variables.
. "$(dirname "$0")/lib.sh"
set -e
minio=${LAYR_MINIO_BIN:-$(command -v minio || true)}
if [ -z "$minio" ] || [ ! -x "$minio" ]; then
    echo 'Set LAYR_MINIO_BIN to the local MinIO executable.' >&2
    exit 1
fi
port=$(python3 - <<'PY'
import socket
s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()
PY
)
export MINIO_ROOT_USER=layr-profile-test MINIO_ROOT_PASSWORD=layr-profile-test-secret
"$minio" server "$T/minio" --address "127.0.0.1:$port" --console-address 127.0.0.1:0 > "$T/minio.log" 2>&1 &
minio_pid=$!
stop_profile_test() { kill "$minio_pid" 2>/dev/null || true; wait "$minio_pid" 2>/dev/null || true; cleanup; }
trap stop_profile_test EXIT
endpoint=http://127.0.0.1:$port
for attempt in $(seq 1 100); do
    if curl -fsS "$endpoint/minio/health/live" >/dev/null 2>&1; then break; fi
    sleep .1
done
curl -fsS --aws-sigv4 aws:amz:us-east-1:s3 --user "$MINIO_ROOT_USER:$MINIO_ROOT_PASSWORD" -X PUT "$endpoint/layr-profile" >/dev/null
export AWS_CONFIG_FILE=$T/aws-config AWS_SHARED_CREDENTIALS_FILE=$T/aws-credentials
printf '[profile test]\nregion=us-east-1\n[profile refuse]\ncredential_process=echo never\n' > "$AWS_CONFIG_FILE"
printf '[test]\naws_access_key_id=%s\naws_secret_access_key=%s\n' "$MINIO_ROOT_USER" "$MINIO_ROOT_PASSWORD" > "$AWS_SHARED_CREDENTIALS_FILE"
chmod 600 "$AWS_CONFIG_FILE" "$AWS_SHARED_CREDENTIALS_FILE"
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_DEFAULT_PROFILE
export AWS_PROFILE=test AWS_ENDPOINT_URL_S3=$endpoint
sample "$T/source"
l init aws-profile --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/aws-profile/lines/main"
ok 'AWS_PROFILE creates an S3 remote' l remote add cloud s3://layr-profile/store --paper-key-out "$T/paper"
ok 'AWS_PROFILE pushes signed states' l push cloud -q
ok 'AWS_PROFILE reads and checks S3 files' l fsck --remote cloud --files
unset AWS_PROFILE
l config remote.cloud.aws-profile test
ok 'the remote profile supplies S3 credentials' l fetch cloud -q
export AWS_PROFILE=refuse
export AWS_ACCESS_KEY_ID=$MINIO_ROOT_USER AWS_SECRET_ACCESS_KEY=$MINIO_ROOT_PASSWORD
ok 'environment keys have priority over the profile' l fetch cloud -q
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
if l fetch cloud -q > "$T/refuse.out" 2> "$T/refuse.err"; then
    eq 'credential_process is refused' refused accepted
else
    has 'credential_process has a clear error' 'credential_process is not supported' "$(cat "$T/refuse.err")"
fi
unset AWS_PROFILE
ok 'the named profile still pushes' l push cloud -q
l op export > "$T/records"
python3 - "$T/records" "$T/aws-credentials" <<'PY'
import pathlib,sys
records=pathlib.Path(sys.argv[1]).read_bytes()
for line in pathlib.Path(sys.argv[2]).read_bytes().splitlines():
    if b'=' in line:
        assert line.split(b'=',1)[1] not in records, 'AWS credential entered records'
PY
eq 'records contain no AWS credentials' 0 0
if [ "$(uname -s)" = Linux ]; then
    for flag in --unlock --lock --status; do
        if l mount "$flag" cloud > "$T/keychain.out" 2>&1; then code=0; else code=$?; fi
        eq "Linux $flag exits 2" 2 "$code"
        has "Linux $flag explains Keychain support" 'the Keychain is macOS only' "$(cat "$T/keychain.out")"
    done
fi
finish
