#!/bin/bash
# Deterministic OCI images and Helm artifacts through real registry clients.
. "$(dirname "$0")/lib.sh"
set -e
for c in docker-registry skopeo helm htpasswd curl python3; do
    command -v "$c" >/dev/null || { echo "FATAL: install $c for t30-oci"; exit 1; }
done
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
chmod 755 "$T"
REGPORT=$(free_port)
PROXYPORT=$(free_port)
LAYRPORT=$(free_port)
htpasswd -Bbn test test > "$T/htpasswd"
cat > "$T/registry.yaml" <<EOF
version: 0.1
log:
  level: error
storage:
  filesystem:
    rootdirectory: $T/registry
http:
  addr: 127.0.0.1:$REGPORT
  secret: test-only-local-registry
auth:
  htpasswd:
    realm: test
    path: $T/htpasswd
EOF
docker-registry serve "$T/registry.yaml" > "$T/registry.log" 2>&1 &
RPID=$!
python3 "$(dirname "$0")/oci-auth-proxy.py" "$PROXYPORT" "$REGPORT" > "$T/proxy.log" 2>&1 &
PPID_OCI=$!
DPID=""
trap 'kill ${DPID:-} $RPID $PPID_OCI 2>/dev/null || true; if [ "$FAIL" -ne 0 ]; then cat "$T/daemon.log" "$T/registry.log"; fi; cleanup' EXIT
for i in $(seq 1 100); do curl -s -u test:test "http://127.0.0.1:$REGPORT/v2/" >/dev/null && break; sleep .1; done
mkdir -p "$T/src/bin" "$T/src/etc" "$T/src/deployment/templates"
printf '#!/bin/sh\necho hello\n' > "$T/src/bin/app"
chmod 755 "$T/src/bin/app"
printf 'answer=42\n' > "$T/src/etc/config"
cat > "$T/src/layr.toml" <<'EOF'
[runnable.app]
layers = [["bin"], ["etc"]]
entrypoint = ["/bin/app"]
working_dir = "/"
ports = ["8080/tcp"]
env = { MODE = "test" }
platform = { os = "linux", arch = "arm64" }
chart = "deployment"
EOF
printf 'apiVersion: v2\nname: app\nversion: 1.2.3\n' > "$T/src/deployment/Chart.yaml"
printf 'image:\n  repository: example/app\n  digest: old\n' > "$T/src/deployment/values.yaml"
printf 'image: {{ .Values.image.repository }}@{{ .Values.image.digest }}\n' > "$T/src/deployment/templates/image.yaml"
l init oci --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/oci/lines/main"
l remote add external "oci://127.0.0.1:$REGPORT/oci/app"
l config remote.external.insecure true
l config remote.external.username test
l config remote.external.password test
FIRST=$(l push external main --runnable app --json)
IMAGE=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["image"])' "$FIRST")
CHART=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["chart"])' "$FIRST")
ok "the first push uploads blobs" python3 -c 'import json,sys; assert json.loads(sys.argv[1])["uploaded_blobs"] > 0' "$FIRST"
printf 'uncommitted\n' > bin/app
DERIVED_BEFORE=$(l ref list derived/oci-layer-v1/ --json)
l gc -q
ok "Core refs keep derived layer blobs through GC" python3 -c 'import json,pathlib,sys; assert all((pathlib.Path(sys.argv[1])/r["target"]/"blob").is_file() for r in json.loads(sys.argv[2]))' "$LAYR_ROOT/projects/oci/states" "$DERIVED_BEFORE"
SECOND=$(l push --runnable app external main --json)
eq "repeat export reuses Core derived states" "$DERIVED_BEFORE" "$(l ref list derived/oci-layer-v1/ --json)"
ok "layer blobs are detached derived states with source metadata" python3 -c 'import json,sqlite3,sys; db=sqlite3.connect(sys.argv[1]); refs=json.loads(sys.argv[2]); assert len(refs)==3; records=[json.loads(db.execute("select rec from states where id=?",(r["target"],)).fetchone()[0]) for r in refs]; assert all(r["kind"]=="derived" and r["context"]=="oci-layer-v1" and r["derived_from"] and not r.get("line") and not r["parents"] for r in records)' "$LAYR_ROOT/projects/oci/meta/db/layr.sqlite" "$DERIVED_BEFORE"
eq "repeat push skips all blobs" "0" "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["uploaded_blobs"])' "$SECOND")"
eq "push reads the revision, not the working folder" "$IMAGE" "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["image"])' "$SECOND")"
HUMAN=$(l push external main --runnable app)
has "the default output names the runnable and tag" 'Pushed app:line-main' "$HUMAN"
has "the default output shows the image digest" "$IMAGE" "$HUMAN"
has "the default output shows skipped blobs" 'Blobs: 0 uploaded, 5 skipped' "$HUMAN"
ok "skopeo pulls all image blobs from Distribution" skopeo copy --src-creds test:test --src-tls-verify=false "docker://127.0.0.1:$REGPORT/oci/app:line-main" "dir:$T/external-image"
eq "the external registry preserves the manifest digest" "$IMAGE" "sha256:$(skopeo inspect --creds test:test --tls-verify=false --raw "docker://127.0.0.1:$REGPORT/oci/app:line-main" | sha256sum | cut -d' ' -f1)"
export HELM_REGISTRY_CONFIG="$T/helm-auth.json"
printf 'test\n' | helm registry login "127.0.0.1:$REGPORT" --username test --password-stdin --insecure >/dev/null 2>&1
ok "Helm pulls the external chart" helm pull "oci://127.0.0.1:$REGPORT/oci/charts/app" --version 1.2.3 --plain-http --destination "$T"
VALUES=$(tar -xzOf "$T/app-1.2.3.tgz" app/values.yaml)
has "the chart uses the same image digest" "$IMAGE" "$VALUES"
l remote add bearer "oci://127.0.0.1:$PROXYPORT/oci/app"
l config remote.bearer.insecure true
l config remote.bearer.username test
l config remote.bearer.password test
BEARER=$(l push bearer main --runnable app --json)
eq "Bearer auth exports the same digest" "$IMAGE" "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["image"])' "$BEARER")"
l config registry.listen "127.0.0.1:$LAYRPORT"
l config registry.anonymous false
l config registry.user root
ROOT_START=$("$LAYR" daemon --socket "$T/refused.sock" 2>&1 || true)
has "registry startup refuses uid zero" 'registry.user must be unprivileged' "$ROOT_START"
l config registry.user nobody
l ref set env/prod main >/dev/null
l ref set release/1.2.3 main >/dev/null
CHANGE=$(l change list --json | python3 -c 'import json,sys; v=json.load(sys.stdin); print(v[0]["change_id"] if isinstance(v[0],dict) else v[0])')
REF_PUSH=$(l push external env/prod --runnable app --json)
eq "external push accepts an environment ref" "$IMAGE" "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["image"])' "$REF_PUSH")"

id layr-oci-reader >/dev/null 2>&1 || useradd -m layr-oci-reader
id layr-oci-denied >/dev/null 2>&1 || useradd -m layr-oci-denied
l access grant '*' none
l access grant layr-oci-reader reader
TOKEN=$(l access token layr-oci-reader --until 1h)
DENIED=$(l access token layr-oci-denied --until 1h)
export LAYR_SOCKET="$T/layr.sock"
cp "$LAYR" "$T/layr-http"
chmod 755 "$T/layr-http"
"$T/layr-http" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep .1; done
ok "the HTTP parser uses an unprivileged uid before request bytes" python3 - "$DPID" "$LAYRPORT" <<'PYTEST'
import os, socket, sys, time
parent, port = sys.argv[1], int(sys.argv[2])
sock = socket.create_connection(('127.0.0.1', port))
found = False
for _ in range(100):
    pids = open(f'/proc/{parent}/task/{parent}/children').read().split()
    for pid in pids:
        try:
            command = open(f'/proc/{pid}/cmdline','rb').read()
            if b'__registry-http' not in command:
                continue
            status = open(f'/proc/{pid}/status').read().splitlines()
            uids = next(x for x in status if x.startswith('Uid:')).split()[1:]
            assert all(int(uid) != 0 for uid in uids), uids
            gids = next(x for x in status if x.startswith('Gid:')).split()[1:]
            assert all(int(gid) != 0 for gid in gids), gids
            assert '0' not in next(x for x in status if x.startswith('Groups:')).split()[1:]
            found = True
        except FileNotFoundError:
            pass
    if found:
        break
    time.sleep(.02)
assert found, 'no unprivileged registry child'
sock.close()
PYTEST
ok "oversized and malformed HTTP requests fail without a daemon crash" python3 - "$LAYRPORT" <<'PYTEST'
import socket,sys
port=int(sys.argv[1])
for request in [b'INVALID\r\n\r\n', b'GET /v2/ HTTP/1.1\r\nX-Long: '+b'x'*20000+b'\r\n\r\n', b'GET /'+b'x'*5000+b' HTTP/1.1\r\n\r\n']:
    with socket.create_connection(('127.0.0.1',port)) as s:
        s.sendall(request)
        response=s.makefile("rb").readline()
        assert response.startswith(b'HTTP/1.1 400'), response
PYTEST
no "the registry identity cannot use ordinary CLI RPC" runuser -u nobody -- env LAYR_SOCKET="$LAYR_SOCKET" "$T/layr-http" --project oci projects
URL="http://127.0.0.1:$LAYRPORT/v2/oci/app"
eq "anonymous pull is disabled" 401 "$(curl -s -o /dev/null -w '%{http_code}' "$URL/manifests/line-main")"
eq "a token without the reader role is refused" 403 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $DENIED" "$URL/manifests/line-main")"
eq "registry writes are refused" 405 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer $TOKEN" "$URL/manifests/line-main")"
LOCAL=$(curl -fsS -H "Authorization: Bearer $TOKEN" "$URL/manifests/line-main")
eq "the Layr registry serves the same image" "$IMAGE" "sha256:$(printf '%s' "$LOCAL" | sha256sum | cut -d' ' -f1)"
ok "skopeo pulls all blobs through Bearer auth from Layr" skopeo copy --src-creds "reader:$TOKEN" --src-tls-verify=false "docker://127.0.0.1:$LAYRPORT/oci/app@$IMAGE" "dir:$T/layr-image"
for TAG in prod 1.2.3 "change-$CHANGE"; do
    MANIFEST=$(curl -fsS -H "Authorization: Bearer $TOKEN" "$URL/manifests/$TAG")
    eq "Core tag $TAG resolves to the same image" "$IMAGE" "sha256:$(printf '%s' "$MANIFEST" | sha256sum | cut -d' ' -f1)"
done
TAGS=$(curl -fsS -H "Authorization: Bearer $TOKEN" "$URL/tags/list")
has "tag discovery lists lines" 'line-main' "$TAGS"
has "tag discovery lists environment refs" 'prod' "$TAGS"
has "tag discovery lists release refs" '1.2.3' "$TAGS"
has "tag discovery lists changes" "change-$CHANGE" "$TAGS"
STATE=$(l rev-parse main)
eq "states resolve without a line" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/manifests/state-$STATE")"
mkdir "$T/release-chart"
ok "Helm pulls the Layr release tag" helm pull "oci://127.0.0.1:$LAYRPORT/oci/charts/app" --version 1.2.3 --plain-http --username reader --password "$TOKEN" --destination "$T/release-chart"
# The chart manifest already exists in the OCI content cache after push.
mkdir "$T/layr-chart"
ok "Helm pulls the Layr chart by digest" helm pull "oci://127.0.0.1:$LAYRPORT/oci/charts/app@$CHART" --plain-http --username reader --password "$TOKEN" --destination "$T/layr-chart"
has "the Layr chart binds the same image" "$IMAGE" "$(python3 -c 'import glob,tarfile,sys; p=glob.glob(sys.argv[1]+"/*.tgz"); assert len(p)==1; print(tarfile.open(p[0]).extractfile("app/values.yaml").read().decode())' "$T/layr-chart")"
eq "a digest from another repository cannot expose a blob" 404 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$LAYRPORT/v2/oci/other/blobs/$(printf '%s' "$LOCAL" | python3 -c 'import json,sys; print(json.load(sys.stdin)["layers"][0]["digest"])')")"
l config secret.paths '["etc"]'
eq "a new secret policy blocks cached manifest access" 403 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/manifests/$IMAGE")"
l config --unset secret.paths
l access token --revoke layr-oci-reader
eq "revocation applies to cached manifests" 401 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/manifests/$IMAGE")"
l config registry.anonymous true
l access grant '*' reader
eq "anonymous pull needs explicit config and reader policy" 200 "$(curl -s -o /dev/null -w '%{http_code}' "$URL/manifests/line-main")"
l access grant '*' none
eq "the anonymous reader policy is checked again" 401 "$(curl -s -o /dev/null -w '%{http_code}' "$URL/manifests/$IMAGE")"
l config secret.paths '["etc"]'
no "export refuses declared secret paths" l push external main --runnable app
l config --unset secret.paths
l overlay create private -q
l overlay use private -q
no "export refuses a view with private overlays" l push external main --runnable app
finish
