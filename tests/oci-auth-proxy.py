#!/usr/bin/env python3
"""Bearer challenge fixture in front of a real Distribution registry."""
import base64
import http.client
import http.server
import json
import sys
from urllib.parse import urlsplit

listen, backend = map(int, sys.argv[1:])
credential = 'Basic ' + base64.b64encode(b'test:test').decode()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, data, headers=None):
        self.send_response(status)
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(data)

    def request(self):
        if urlsplit(self.path).path == '/token':
            if self.headers.get('Authorization') != credential:
                return self.reply(401, b'')
            return self.reply(200, json.dumps({'token': 'fixture-token'}).encode(), {'Content-Type': 'application/json'})
        if self.headers.get('Authorization') != 'Bearer fixture-token':
            repository = self.path.removeprefix('/v2/').split('/blobs')[0].split('/manifests')[0]
            return self.reply(401, b'', {'WWW-Authenticate': f'Bearer realm="http://127.0.0.1:{listen}/token",service="fixture",scope="repository:{repository}:pull,push"'})
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'authorization', 'connection')}
        headers['Authorization'] = credential
        connection = http.client.HTTPConnection('127.0.0.1', backend, timeout=30)
        connection.request(self.command, self.path, body=body, headers=headers)
        response = connection.getresponse()
        data = response.read()
        headers = {k: v for k, v in response.getheaders() if k.lower() not in ('content-length', 'connection', 'server', 'date')}
        for k in list(headers):
            if k.lower() == 'location':
                headers[k] = headers[k].replace(f'127.0.0.1:{backend}', f'127.0.0.1:{listen}')
        self.reply(response.status, data, headers)
        connection.close()

    do_GET = do_HEAD = do_POST = do_PUT = request


http.server.ThreadingHTTPServer(('127.0.0.1', listen), Handler).serve_forever()
