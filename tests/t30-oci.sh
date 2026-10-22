#!/bin/bash
# Deterministic OCI images and Helm artifacts through real registry clients.
. "$(dirname "$0")/lib.sh"
set -e
for c in docker-registry skopeo helm htpasswd curl python3; do
    command -v "$c" >/dev/null || { echo "FATAL: install $c for t30-oci"; exit 1; }
done
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
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
SECOND=$(l push --runnable app external main --json)
eq "repeat push skips all blobs" "0" "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["uploaded_blobs"])' "$SECOND")"
eq "push reads the revision, not the working folder" "$IMAGE" "$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["image"])' "$SECOND")"
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
id layr-oci-reader >/dev/null 2>&1 || useradd -m layr-oci-reader
id layr-oci-denied >/dev/null 2>&1 || useradd -m layr-oci-denied
l access grant '*' none
l access grant layr-oci-reader reader
TOKEN=$(l access token layr-oci-reader --until 1h)
DENIED=$(l access token layr-oci-denied --until 1h)
export LAYR_SOCKET="$T/layr.sock"
"$LAYR" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep .1; done
URL="http://127.0.0.1:$LAYRPORT/v2/oci/app"
eq "anonymous pull is disabled" 401 "$(curl -s -o /dev/null -w '%{http_code}' "$URL/manifests/line-main")"
eq "a token without the reader role is refused" 403 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $DENIED" "$URL/manifests/line-main")"
eq "registry writes are refused" 405 "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer $TOKEN" "$URL/manifests/line-main")"
LOCAL=$(curl -fsS -H "Authorization: Bearer $TOKEN" "$URL/manifests/line-main")
eq "the Layr registry serves the same image" "$IMAGE" "sha256:$(printf '%s' "$LOCAL" | sha256sum | cut -d' ' -f1)"
ok "skopeo pulls all blobs through Bearer auth from Layr" skopeo copy --src-creds "reader:$TOKEN" --src-tls-verify=false "docker://127.0.0.1:$LAYRPORT/oci/app@$IMAGE" "dir:$T/layr-image"
TAGS=$(curl -fsS -H "Authorization: Bearer $TOKEN" "$URL/tags/list")
has "tag discovery lists lines" 'line-main' "$TAGS"
STATE=$(l rev-parse main)
eq "states resolve without a line" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/manifests/state-$STATE")"
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
