"""Exercise version negotiation and refusal through the real daemon socket."""
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
store = root / "store"
env = dict(os.environ, LAYR_ROOT=str(store), LAYR_DIRECT="1")
env.pop("LAYR_SOCKET", None)
current = json.loads(subprocess.check_output([binary, "store-version", "--json"], env=env))["binary"]


def exact(sock, size):
    result = b""
    while len(result) < size:
        part = sock.recv(size - len(result))
        assert part, "short daemon response"
        result += part
    return result


def request(version, hello=False):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(15)
        sock.connect(str(root / "version.sock"))
        if hello:
            packet = b'{"hello":true}'
            sock.sendall(struct.pack("!I", len(packet)) + packet)
            assert exact(sock, 1) == b"V"
            length = struct.unpack("!I", exact(sock, 4))[0]
            assert json.loads(exact(sock, length)) == current
        payload = {"argv": ["init", "must-not-exist", "-q"], "env": {}}
        if version is not None:
            payload["version"] = version
        if version is not None and version["format"] == current["format"]:
            payload["argv"] = ["projects"]
        packet = json.dumps(payload).encode()
        with open(os.devnull) as stdin, tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            directory = os.open(root, os.O_RDONLY)
            try:
                fds = array.array("i", [stdin.fileno(), stdout.fileno(), stderr.fileno(), directory])
                sock.sendmsg([struct.pack("!I", len(packet)) + packet], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, fds)])
                assert exact(sock, 1) == b"R"
                code = struct.unpack("!i", exact(sock, 4))[0]
                stderr.seek(0)
                error = stderr.read().decode()
            finally:
                os.close(directory)
    return code, error


for hello in (False, True):
    other = dict(current, format=current["format"] + 1, version="future-version", build="future-build")
    code, error = request(other, hello)
    assert code == 128, error
    for text in ("format 2", "format 1", "future-version", "future-build", current["version"], current["build"]):
        assert text in error, (text, error)
    assert not (store / "projects/must-not-exist").exists()
    code, error = request(None, hello)
    assert code == 128 and "unguarded layr" in error, error
    assert not (store / "projects/must-not-exist").exists()
    code, error = request(dict(current, version="another-version", build="another-build"), hello)
    assert code == 0, error
print("PASS the daemon refuses different formats and legacy clients before work", flush=True)
result = subprocess.run([binary, "store-version", "--upgrade"], env=env, capture_output=True, text=True)
assert result.returncode == 128 and "in use" in result.stderr, result.stderr
print("PASS the daemon holds its format lease for its lifetime", flush=True)
print("t54-store-version-daemon.sh: 2 passed, 0 failed")
