#!/usr/bin/env python3
"""A headless Mac joins without installing or starting a LaunchAgent."""
import http.server
import json
import os
import stat
import subprocess
import sys
import tempfile
import threading
from pathlib import Path


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        assert self.path == '/v1/runners/join'
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        assert body['platform'] == 'macos-aarch64'
        assert body['code'] == 'testcode'
        assert body['projects'] == ['layr', 'layr-cloud']
        reply = json.dumps({'runner': 'test-runner', 'grant': 'run1.test.9999999999999.nonce.mac',
                            'projects': body['projects']}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *_):
        pass


def main():
    if os.getuid() == 0:
        raise RuntimeError('run as a non-root user')
    with tempfile.TemporaryDirectory(prefix='layr-runner-headless-') as name:
        root = Path(name)
        home = root / 'home'
        home.mkdir()
        store = root / 'store'
        server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            env = dict(os.environ, HOME=str(home), LAYR_ROOT=str(store))
            env.pop('LAYR_SOCKET', None)
            url = f'http://127.0.0.1:{server.server_port}'
            code = root / 'join-code'
            code.write_text('testcode\n')
            code.chmod(0o644)
            args = [sys.argv[1], 'checks', 'join', url, '--code-file', str(code),
                    '--project', 'layr,layr-cloud', '--no-start', '--json']
            refused = subprocess.run(args, env=env, text=True, capture_output=True, timeout=30)
            assert refused.returncode != 0 and 'private regular file' in refused.stderr
            code.chmod(0o600)
            link = root / 'join-code-link'
            link.symlink_to(code)
            link_args = [str(link) if x == str(code) else x for x in args]
            refused = subprocess.run(link_args, env=env, text=True, capture_output=True, timeout=30)
            assert refused.returncode != 0
            result = subprocess.run(args,
                                    env=env, text=True, capture_output=True, timeout=30, check=True)
            reply = json.loads(result.stdout)
            assert reply['projects'] == ['layr', 'layr-cloud']
            assert reply['daemon'] == 'start `layr daemon` as the store user'
            token = store / 'runner/token'
            assert stat.S_IMODE(token.stat().st_mode) == 0o600
            assert not (home / 'Library/LaunchAgents').exists()
            print('PASS: headless join uses a private code file, saves a private token and starts no LaunchAgent')
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == '__main__':
    main()
