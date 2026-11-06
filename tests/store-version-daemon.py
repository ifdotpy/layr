"""Exercise version negotiation and refusal through the real daemon socket."""
import array
import fcntl
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


# The marker/report keeps Version; negotiation adds the required schema in Peer.
with socket.socket(socket.AF_UNIX) as probe:
    probe.settimeout(15)
    probe.connect(str(root / "version.sock"))
    packet = b'{"hello":true}'
    probe.sendall(struct.pack("!I", len(packet)) + packet)
    assert exact(probe, 1) == b"V"
    reply = json.loads(exact(probe, struct.unpack("!I", exact(probe, 4))[0]))
    assert {key: reply[key] for key in current} == current, reply
    assert type(reply["schema"]) is int and reply["schema"] > 0, reply
    current = dict(current, schema=reply["schema"])


def request(version, hello=False):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(15)
        sock.connect(str(root / "version.sock"))
        if hello:
            packet = b'{"hello":true}'
            sock.sendall(struct.pack("!I", len(packet)) + packet)
            assert exact(sock, 1) == b"V"
            length = struct.unpack("!I", exact(sock, 4))[0]
            # The reply is the version plus the store root of the daemon.
            reply = json.loads(exact(sock, length))
            assert {k: reply[k] for k in current} == current, reply
            assert Path(reply["root"]).resolve() == store.resolve(), reply
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
    for schema in (None, 0, current["schema"] + 1):
        peer = dict(current)
        if schema is None:
            peer.pop("schema")
        else:
            peer["schema"] = schema
        code, error = request(peer, hello)
        assert code == 128 and "upgrade both the CLI and the daemon" in error, error
        assert not (store / "projects/must-not-exist").exists()
    code, error = request(dict(current, version="another-version", build="another-build"), hello)
    assert code == 0, error
print("PASS the daemon refuses different formats and unguarded clients before work", flush=True)
with (store / ".layr-version.lock").open("r+") as lease:
    try:
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        raise AssertionError("the daemon does not hold the format lease")
    except BlockingIOError:
        pass
print("PASS the daemon holds its format lease for its lifetime", flush=True)
print("t54-store-version-daemon.sh: 2 passed, 0 failed")
