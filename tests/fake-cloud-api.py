#!/usr/bin/env python3
"""A small Layr Cloud API v1 server for tests (layr-cloud docs/api.md, layr docs/remotes.md).

Usage: fake-cloud-api.py <folder> <token>. It serves HTTPS on 127.0.0.1 with the certificate in
<folder>/tls-cert.pem, writes its port to <folder>/port, and keeps everything in memory.
Test routes under /test/: POST /test/ref {"name","target"}, POST /test/delete {"id"},
POST /test/minimum {"minimum"}, GET /test/stats.
"""
import hashlib, http.server, json, os, ssl, sys, threading, urllib.parse

folder, token = sys.argv[1], sys.argv[2]
lock = threading.Lock()
db = {'id': None, 'objects': {}, 'uploads': {}, 'segments': {}, 'states': {}, 'refs': {}, 'machines': {},
      'packs': {}, 'located': {}, 'minimum': None, 'missing_project': False, 'public_reads': False,
      'log': [], 'auth': 0, 'anonymous': 0}


def index_entries(index, data):
    """The object ids of a pack index (docs/objects.md, "Packs"), after its checks."""
    import struct
    body, total = index[:-32], index[-32:]
    assert hashlib.sha256(body).digest() == total and body[:4] == b'LPI1'
    pack = body[4:36].hex()
    assert pack == typed('opaque', data), 'the index names another pack'
    nblocks = struct.unpack_from('<I', body, 36)[0]
    at = 40 + nblocks * 16
    nentries = struct.unpack_from('<I', body, at)[0]
    at += 4
    return [body[at + i * 46: at + i * 46 + 32].hex() for i in range(nentries)]


def typed(kind, body):
    return hashlib.sha256(f'{kind} {len(body)}\0'.encode() + body).hexdigest()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def reply(self, status, body=b'', ctype='application/json'):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Content-Type', ctype)
        self.send_header('Layr-Server', 'fake-cloud-1')
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self): self.route()
    def do_POST(self): self.route()
    def do_PUT(self): self.route()

    def route(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        with lock:
            try:
                self.api(raw)
            except Exception as e:  # a test sees the error in the stats
                db['log'].append('error ' + repr(e))
                self.reply(500, {'error': {'code': 'fake', 'message': repr(e)}})

    def api(self, raw):
        url = urllib.parse.urlsplit(self.path)
        path, query = url.path, dict(urllib.parse.parse_qsl(url.query))
        db['log'].append(f'{self.command} {path}')
        body = json.loads(raw) if raw and (self.headers.get('Content-Type', '').startswith('application/json') or path.startswith('/test/')) else {}
        if path.startswith('/test/'):
            if path == '/test/stats':
                return self.reply(200, {'objects': len(db['objects']), 'states': len(db['states']),
                                        'refs': {f'{s}{n}': t for (s, n), t in db['refs'].items()},
                                        'machines': len(db['machines']), 'log': db['log'],
                                        'auth': db['auth'], 'anonymous': db['anonymous']})
            if path == '/test/ref':
                db['refs'][('', body['name'])] = body['target']
            elif path == '/test/delete':
                db['objects'].pop(body['id'])
            elif path == '/test/minimum':
                db['minimum'] = body['minimum']
            elif path == '/test/missing-project':
                db['missing_project'] = body['missing']
            elif path == '/test/public-reads':
                db['public_reads'] = body['enabled']
            return self.reply(200, {})
        assert self.headers.get('Layr-Client'), 'every request names its client version'
        if path.startswith('/api/v1/'):
            db['auth' if self.headers.get('Authorization') else 'anonymous'] += 1
        if db['minimum']:
            return self.reply(426, {'error': {'code': 'client_too_old', 'minimum': db['minimum']}})
        read_post = self.command == 'POST' and path.endswith(('/states/missing', '/objects/locate'))
        if self.headers.get('Authorization') != f'Bearer {token}' and not (db['public_reads'] and (self.command == 'GET' or read_post)):
            return self.reply(401, {'error': {'code': 'unauthorized', 'message': 'no token'}})
        if path.startswith('/api/v1/machines/'):
            db['machines'][path.rsplit('/', 1)[1]] = body['public_key']
            return self.reply(200, {})
        prefix = '/api/v1/projects/p'
        if not path.startswith(prefix):
            return self.reply(404, {'error': {'code': 'not_found'}})
        rest = path[len(prefix):]
        if rest == '':
            if db['missing_project']:
                return self.reply(404, {'error': {'code': 'not_found'}})
            return self.reply(200, {'name': 'p', 'mode': 'public' if db['public_reads'] else 'private', 'role': 'admin', 'id': db['id']})
        rest = rest[1:]
        scope = ''
        if rest.startswith('overlays/'):
            name, rest = rest[len('overlays/'):].split('/', 1)
            scope = f'overlays/{name}/'
        parts = rest.split('/')
        m = self.command
        if m == 'POST' and parts == ['objects', 'missing']:
            return self.reply(200, {'missing': [i for i in body['ids'] if i not in db['objects'] and i not in db['located']]})
        if m == 'POST' and parts == ['objects', 'locate']:
            return self.reply(200, {'packs': {i: db['located'][i] for i in body['ids'] if i in db['located']}})
        if parts[0] == 'packs' and len(parts) == 2:
            pid = parts[1]
            if m == 'PUT':
                if pid not in db['objects']:
                    return self.reply(400, {'error': {'code': 'invalid', 'message': 'no pack data'}})
                for oid in index_entries(raw, db['objects'][pid][1]):
                    db['located'][oid] = pid
                db['packs'][pid] = raw
                return self.reply(200, {})
            if pid not in db['packs']:
                return self.reply(404, {'error': {'code': 'not_found'}})
            return self.reply(200, db['packs'][pid], 'application/octet-stream')
        if parts[0] == 'objects' and len(parts) == 2:
            oid = parts[1]
            if m == 'PUT':
                kind = self.headers.get('Layr-Object-Type', '')
                if typed(kind, raw) != oid:
                    return self.reply(400, {'error': {'code': 'invalid'}})
                new = oid not in db['objects']
                db['objects'][oid] = (kind, raw)
                return self.reply(201 if new else 200)
            if oid not in db['objects']:
                return self.reply(404, {'error': {'code': 'not_found'}})
            return self.reply(200, db['objects'][oid][1], 'application/octet-stream')
        if parts[0] == 'records':
            if len(parts) == 1:
                segs = [{'writer': w, 'first': a, 'last': b, 'sha256': '', 'size': 0}
                        for (s, w, a, b) in sorted(db['segments']) if s == scope]
                return self.reply(200, {'segments': segs, 'next': None})
            writer = parts[1]
            first, last = map(int, parts[2].split('-'))
            key = (scope, writer, first, last)
            if m == 'GET':
                if key not in db['segments']:
                    return self.reply(404, {})
                return self.reply(200, db['segments'][key], 'application/octet-stream')
            held = max([b for (s, w, a, b) in db['segments'] if s == scope and w == writer] or [0])
            if first != held + 1:
                return self.reply(409, {'error': {'code': 'conflict'}, 'last': held})
            db['segments'][key] = raw
            if db['id'] is None and scope == '':
                # The first record names the project.
                db['id'] = json.loads(raw.split(b'\n')[0])['r']['project']
            return self.reply(201)
        if m == 'POST' and parts == ['states', 'missing']:
            return self.reply(200, {'missing': [i for i in body['ids'] if (scope, i) not in db['states']]})
        if parts[0] == 'states':
            key = (scope, parts[1])
            if m == 'PUT':
                value = (body.get('tree'), body.get('times'))
                if not all(v in db['located'] for v in value):
                    return self.reply(400, {'error': {'code': 'invalid', 'message': 'tree and times must be in packs'}})
                if key in db['states'] and db['states'][key] != value:
                    return self.reply(409, {'error': {'code': 'conflict'}})
                new = key not in db['states']
                db['states'][key] = value
                return self.reply(201 if new else 200)
            if key not in db['states']:
                return self.reply(404, {'error': {'code': 'not_found'}})
            tree, times = db['states'][key]
            return self.reply(200, {'times': times, 'tree': tree})
        if parts == ['refs']:
            pre = query.get('prefix', '')
            refs = [{'name': n, 'target': t, 'version': 1} for (s, n), t in db['refs'].items()
                    if s == scope and n.startswith(pre)]
            return self.reply(200, {'refs': refs})
        if parts == ['refs', 'cas']:
            conflicts = []
            for u in body['updates']:
                assert u.get('record'), 'each move names its record'
                actual = db['refs'].get((scope, u['name']))
                if actual != u['expect']:
                    conflicts.append({'name': u['name'], 'actual': actual})
            if conflicts:
                return self.reply(409, {'error': {'code': 'conflict'}, 'conflicts': conflicts})
            for u in body['updates']:
                if u['target'] is None:
                    db['refs'].pop((scope, u['name']), None)
                else:
                    db['refs'][(scope, u['name'])] = u['target']
            return self.reply(200, {'refs': [{'name': u['name'], 'target': u['target'], 'version': 1}
                                             for u in body['updates']]})
        return self.reply(404, {'error': {'code': 'not_found'}})


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(os.path.join(folder, 'tls-cert.pem'), os.path.join(folder, 'tls-key.pem'))
server.socket = context.wrap_socket(server.socket, server_side=True)
with open(os.path.join(folder, 'port.tmp'), 'w') as f:
    f.write(str(server.server_address[1]))
os.rename(os.path.join(folder, 'port.tmp'), os.path.join(folder, 'port'))
server.serve_forever()
