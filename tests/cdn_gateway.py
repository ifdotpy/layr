"""Loopback TLS fixture for the real CLI object transport; all keys and bytes are synthetic."""
import collections
import http.server
import json
import pathlib
import ssl
import subprocess
import sys
import time
import urllib.parse

root = pathlib.Path(sys.argv[1])
def openssl(*args):
    subprocess.run(['openssl', *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(root/'ca-key.pem'), '-out', str(root/'ca.pem'), '-days', '1', '-subj', '/CN=CDN test CA')
openssl('req', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(root/'key.pem'), '-out', str(root/'request.csr'), '-subj', '/CN=localhost')
(root/'ext.cnf').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n')
openssl('x509', '-req', '-in', str(root/'request.csr'), '-CA', str(root/'ca.pem'), '-CAkey', str(root/'ca-key.pem'), '-CAcreateserial', '-out', str(root/'cert.pem'), '-days', '1', '-extfile', str(root/'ext.cnf'))
valid = (root/'valid.age').read_bytes()
counts = collections.Counter()
class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def answer(self, data, status=200, kind='application/octet-stream'):
        if not isinstance(data, bytes): data = json.dumps(data).encode()
        self.send_response(status)
        self.send_header('Content-Type',kind)
        self.send_header('Content-Length',str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        parsed = urllib.parse.urlsplit(self.path)
        if parsed.path == '/counts': return self.answer(dict(counts),kind='application/json')
        if parsed.path.startswith('/o/project/files/'):
            assert self.headers.get('Authorization') is None
            assert self.headers.get('x-layr-cdn') is None
            mode = int(parsed.path.split('.')[0][-1])
            counts[f'{mode}.cdn'] += 1
            if mode == 2: return self.answer(b'blocked',403)
            return self.answer(b'x'*len(valid))
        prefix = '/v1/p/project/objects/files/'
        assert parsed.path.startswith(prefix), parsed.path
        assert self.headers.get('Authorization') == 'Bearer fixture.read'
        mode = int(parsed.path.split('.')[0][-1])
        if self.headers.get('x-layr-cdn') == '1':
            counts[f'{mode}.link'] += 1
            expires = int(time.time())+300
            host = 'other.invalid' if mode == 3 else f'localhost:{self.server.server_port}'
            key = parsed.path[len('/v1/p/project/objects/'):]
            return self.answer({'url':f'https://{host}/o/project/{key}?Expires={expires}&Signature=fixture&Key-Pair-Id=K123&Hash-Algorithm=SHA256','expires':expires,'bytes':len(valid)},kind='application/vnd.layr.cdn+json')
        counts[f'{mode}.direct'] += 1
        return self.answer(b'x'*len(valid) if mode == 4 else valid)
server = http.server.ThreadingHTTPServer(('127.0.0.1',0),Fixture)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root/'cert.pem',root/'key.pem')
server.socket = context.wrap_socket(server.socket,server_side=True)
print(server.server_port,flush=True)
server.serve_forever()
