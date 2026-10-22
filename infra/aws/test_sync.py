"""Local release and configuration checks. No AWS requests or systemd changes."""

import hashlib
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("cloud_sync", Path(__file__).with_name("sync.py"))
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)


def program(interpreter=False):
    data = bytearray(128)
    data[:6] = b"\x7fELF\x02\x01"
    data[18:20] = (183).to_bytes(2, "little")
    data[32:40] = (64).to_bytes(8, "little")
    data[54:56] = (56).to_bytes(2, "little")
    data[56:58] = (1).to_bytes(2, "little")
    data[64:68] = (3 if interpreter else 1).to_bytes(4, "little")
    return bytes(data)


def release(binary=None, extra=None):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w:gz") as archive:
        files = {"./layr": binary if binary is not None else program(), "./sync.py": b"print('sync')\n"}
        if extra:
            files.update(extra)
        for name, content in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(content)
            archive.addfile(member, io.BytesIO(content))
    data = stream.getvalue()
    return data, hashlib.sha256(data).hexdigest()


class ReleaseTests(unittest.TestCase):
    def test_static_arm64_release(self):
        data, digest = release()
        self.assertEqual(set(sync.release_files(data, digest)), {"layr", "sync.py"})

    def test_corrupt_release(self):
        data, digest = release()
        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            sync.release_files(data + b"corrupt", digest)

    def test_dynamic_program(self):
        data, digest = release(program(interpreter=True))
        with self.assertRaisesRegex(ValueError, "must be static"):
            sync.release_files(data, digest)

    def test_wrong_architecture(self):
        binary = bytearray(program())
        binary[18:20] = (62).to_bytes(2, "little")
        data, digest = release(bytes(binary))
        with self.assertRaisesRegex(ValueError, "ARM64"):
            sync.release_files(data, digest)

    def test_path_escape(self):
        data, digest = release(extra={"../../etc/evil": b"bad"})
        with self.assertRaisesRegex(ValueError, "only regular"):
            sync.release_files(data, digest)

    def test_link(self):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w:gz") as archive:
            member = tarfile.TarInfo("./layr")
            member.type = tarfile.SYMTYPE
            member.linkname = "/bin/sh"
            archive.addfile(member)
        data = stream.getvalue()
        with self.assertRaisesRegex(ValueError, "only regular"):
            sync.release_files(data, hashlib.sha256(data).hexdigest())

    def test_atomic_install_and_previous_release_survives_bad_download(self):
        with tempfile.TemporaryDirectory() as temporary:
            host = sync.Sync.__new__(sync.Sync)
            host.kind = "gateway"
            host.app = Path(temporary) / "app"
            host.restart = False
            data, digest = release()
            manifest = {"key": f"gateway/releases/{digest}.tgz", "sha256": digest}
            host.get = Mock(side_effect=lambda key: json.dumps(manifest).encode() if key.endswith(".json") else data)
            with patch.object(sync, "write_if_changed"):
                host.release()
                self.assertEqual((host.app / "current/layr").read_bytes(), program())
                current = (host.app / "current").resolve()
                data2, digest2 = release(extra={"extra": b"invalid"})
                manifest.update(key=f"gateway/releases/{digest2}.tgz", sha256=digest2)
                data = data2
                with self.assertRaises(ValueError):
                    host.release()
                self.assertEqual((host.app / "current").resolve(), current)

    def test_restart_retries_after_failed_activation(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            host = sync.Sync.__new__(sync.Sync)
            host.kind = "gateway"
            host.home = home / "etc"
            host.app = home / "app"
            host.source_path = host.home / "source.json"
            host.restart = False
            (host.app / "current").mkdir(parents=True)
            (host.app / "current/.sha256").write_text("a" * 64)
            def fail_restart(*args, **kwargs):
                if args[:2] == ("systemctl", "restart"):
                    raise RuntimeError("start failed")
                return Mock(stdout="active")
            with patch.object(sync, "UNITS", home / "units"), patch.object(sync, "run", side_effect=fail_restart):
                with self.assertRaisesRegex(RuntimeError, "start failed"):
                    host.services(("/cert", "/key"))
            self.assertFalse((host.home / ".active-release-sha256").exists())
            host.restart = False
            with patch.object(sync, "UNITS", home / "units"), patch.object(sync, "run", return_value=Mock(stdout="active")) as runner:
                host.services(("/cert", "/key"))
                runner.assert_any_call("systemctl", "restart", "layr-gateway.service")
            self.assertEqual((host.home / ".active-release-sha256").read_text(), "a" * 64)


class ConfigTests(unittest.TestCase):
    def test_config_schema_and_local_token_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            host = sync.Sync.__new__(sync.Sync)
            host.home = Path(temporary)
            host.bucket = "artifacts"
            host.restart = False
            host.s3 = Mock()
            host.s3.get_paginator.return_value.paginate.return_value = [
                {"Contents": [{"Key": "gateway/projects/example.json"}]}]
            project = {"bucket": "store", "region": "eu-central-1", "prefix": "example",
                       "readerRoleArn": "reader", "writerRoleArn": "writer", "gcRoleArn": "gc",
                       "recipient": "age1example", "writeTokenHashes": [], "gcTokenHashes": []}
            host.get = lambda key: json.dumps({"host": "store.example.com"} if key.endswith("gateway.json") else project).encode()
            host.configure("100.64.0.1")
            path = host.home / "config.json"
            config = json.loads(path.read_text())
            self.assertEqual(config["listen"], "100.64.0.1:443")
            self.assertEqual(config["projects"]["example"], project)
            config["projects"]["example"]["writeTokenHashes"] = ["a" * 64]
            path.write_text(json.dumps(config))
            host.configure("100.64.0.1")
            self.assertEqual(json.loads(path.read_text())["projects"]["example"]["writeTokenHashes"], ["a" * 64])
            host.configure("100.64.0.2")
            changed = json.loads(path.read_text())
            self.assertEqual(changed["listen"], "100.64.0.2:443")
            self.assertEqual(changed["projects"]["example"]["writeTokenHashes"], ["a" * 64])
            # A new remote config is authoritative, including token revocation.
            project["recipient"] = "age1new"
            host.configure("100.64.0.1")
            self.assertEqual(json.loads(path.read_text())["projects"]["example"]["writeTokenHashes"], [])

    def test_executor_public_identity_has_no_private_fields(self):
        with tempfile.TemporaryDirectory() as temporary:
            host = sync.Sync.__new__(sync.Sync)
            host.home = Path(temporary)
            host.app = Path(temporary) / "app"
            identity = {"id": "019a019a-019a-719a-819a-019a019a019a", "public_key": "a" * 64, "private_key": "secret"}
            with patch.object(sync, "run", return_value=Mock(stdout=json.dumps(identity))) as runner:
                with patch("sys.stdout", new_callable=io.StringIO) as output:
                    host.executor_identity()
                runner.assert_any_call("systemctl", "start", "layr-data.service")
                runner.assert_any_call("/usr/bin/env", "LAYR_ROOT=/var/lib/layr",
                                       str(host.app / "current/layr"), "remote", "self", "--json")
            stored = json.loads((host.home / "machine.json").read_text())
            self.assertEqual(stored, {"id": identity["id"], "public_key": identity["public_key"]})
            self.assertNotIn("secret", output.getvalue())

    def test_executor_invalid_public_identity_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            host = sync.Sync.__new__(sync.Sync)
            host.home = Path(temporary)
            host.app = Path(temporary) / "app"
            identity = {"id": "019a019a-019a-719a-819a-019a019a019a", "public_key": "not a key"}
            with patch.object(sync, "run", return_value=Mock(stdout=json.dumps(identity))):
                with self.assertRaisesRegex(ValueError, "invalid executor public key"):
                    host.executor_identity()
            self.assertFalse((host.home / "machine.json").exists())

    def test_no_server_without_tailnet(self):
        host = sync.Sync.__new__(sync.Sync)
        host.tailnet = Mock(side_effect=ValueError("no tailnet IPv4 address"))
        host.release = Mock()
        host.services = Mock()
        with self.assertRaisesRegex(ValueError, "no tailnet"):
            host.execute()
        host.release.assert_not_called()
        host.services.assert_not_called()



if __name__ == "__main__":
    unittest.main()
