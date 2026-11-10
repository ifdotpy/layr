#!/bin/bash
# ci: macos overlayfs
# V1 discovery can use an existing legacy store, but cannot turn a denial or a missing project into a write.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init p --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$T/key.pem" -out "$T/cert.pem" -days 1 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
    -addext basicConstraints=critical,CA:FALSE >/dev/null 2>&1
export V1_BRIDGE_T="$T" LAYR_GATEWAY_CA_FILE="$T/cert.pem" LAYR
python3 - <<'PY'
import http.server, json, os, pathlib, ssl, subprocess, threading

root = pathlib.Path(os.environ['V1_BRIDGE_T'])
binary = os.environ['LAYR']
descriptor = (root / 'archive/store.json').read_bytes()
calls = []

class Gateway(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        calls.append(('GET', self.path))
        status, data = 404, b'{}'
        if self.path == '/api/v1/projects/denied':
            status = 403
        elif self.path == '/api/v1/projects/legacy':
            status = 200
            data = json.dumps({'id': json.loads(descriptor)['project'], 'mode': 'private'}).encode()
        elif self.path == '/api/v1/projects/legacy/records?limit=1':
            data = b'{"error":{"code":"not_found","message":"no such route"}}'
        elif self.path == '/api/v1/projects/hidden':
            status = 200
            data = json.dumps({'id': json.loads(descriptor)['project'], 'mode': 'private'}).encode()
        elif self.path == '/api/v1/projects/hidden/records?limit=1':
            data = b'{"error":{"code":"not_found","message":"no such project"}}'
        elif self.path == '/v1/p/legacy/store.json':
            status, data = 200, descriptor
        self.send_response(status)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_PUT(self):
        calls.append(('PUT', self.path))
        self.send_error(500)
    def do_POST(self):
        calls.append(('POST', self.path))
        self.send_error(500)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Gateway)
tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
tls.load_cert_chain(root / 'cert.pem', root / 'key.pem')
server.socket = tls.wrap_socket(server.socket, server_side=True)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
base = f'layr+https://localhost:{server.server_port}/'

def run(*args):
    return subprocess.run([binary, *args], capture_output=True, text=True, timeout=30)

try:
    result = run('remote', 'add', 'legacy', base + 'legacy')
    assert result.returncode == 0, result.stderr
    assert ('GET', '/api/v1/projects/legacy') in calls
    assert ('GET', '/api/v1/projects/legacy/records?limit=1') in calls
    assert ('GET', '/v1/p/legacy/store.json') in calls
    mode = run('config', '--get', 'cloud.remote.legacy.mode')
    assert mode.returncode == 0 and mode.stdout.strip() == 'secret', mode.stderr
    calls.clear()
    result = run('remote', 'add', 'denied', base + 'denied')
    assert result.returncode != 0 and 'HTTP 403' in result.stderr, result.stderr
    assert not any(path.startswith('/v1/') for _, path in calls), calls
    calls.clear()
    result = run('remote', 'add', 'hidden', base + 'hidden')
    assert result.returncode != 0 and 'HTTP 404' in result.stderr, result.stderr
    assert not any(path.startswith('/v1/') for _, path in calls), calls
    calls.clear()
    result = run('remote', 'add', 'missing', base + 'missing')
    assert result.returncode != 0, result.stdout
    assert not any(method in ('PUT', 'POST') for method, _ in calls), calls
    print('PASS legacy detection, access denial, and missing-project safety')
finally:
    server.shutdown()
    server.server_close()
    thread.join()
PY
PASS=$((PASS + 1))
finish
