"""Send client environments and stdio descriptors to an isolated test daemon."""
import array
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()


def exact(sock, size):
    result = b""
    while len(result) < size:
        part = sock.recv(size - len(result))
        assert part, "short daemon response"
        result += part
    return result


def request(environment):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(15)
        sock.connect(str(root / "trace.sock"))
        hello = b'{"hello":true}'
        sock.sendall(struct.pack("!I", len(hello)) + hello)
        assert exact(sock, 1) == b"V"
        size = struct.unpack("!I", exact(sock, 4))[0]
        # The daemon's own identity (format, version, build and database schema) is the peer the client sends.
        peer = json.loads(exact(sock, size))
        peer.pop("root", None)
        payload = json.dumps({
            "argv": ["volume", "list", "--json"], "cwd": str(root),
            "env": environment, "version": peer,
        }).encode()
        with open(os.devnull) as stdin, tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            directory = os.open(root, os.O_RDONLY)
            try:
                fds = array.array("i", [stdin.fileno(), stdout.fileno(), stderr.fileno(), directory])
                sock.sendmsg([struct.pack("!I", len(payload)) + payload], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, fds)])
                assert exact(sock, 1) == b"R"
                code = struct.unpack("!i", exact(sock, 4))[0]
                stdout.seek(0)
                stderr.seek(0)
                return code, stdout.read(), stderr.read().decode()
            finally:
                os.close(directory)


def stable_json(data):
    rows = json.loads(data)
    for row in rows:
        # Concurrent CI writes can change the available disk space between requests.
        row.pop("used")
        row.pop("free")
    return rows


code, traced, error = request({"LAYR_TRACE": "1"})
assert code == 0, error
assert any(line.startswith("trace ") for line in error.splitlines()), error
code, plain, error = request({})
assert code == 0, error
assert not any(line.startswith("trace ") for line in error.splitlines()), error
assert stable_json(traced) == stable_json(plain)
for environment in ({"LAYR_TRACE": 1}, [], None):
    code, output, error = request(environment)
    assert code == 128 and not output and error, (code, output, error)
print("PASS daemon request tracing is isolated and JSON stdout is unchanged")
print("PASS malformed client environments fail instead of becoming empty")
print("t177-service-trace.sh: 2 passed, 0 failed")
