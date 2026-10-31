"""Exercise the public commands for the 0.2.1 regressions."""
import fcntl
import os
from pathlib import Path
import pwd
import shlex
import sqlite3
import subprocess
import sys
import time

binary = str(Path(sys.argv[1]).resolve())
root = Path(sys.argv[2]).resolve()
env = dict(os.environ, LAYR_ROOT=str(root / "store"))
for key in ("LAYR_SOCKET", "LAYR_PROJECT", "LAYR_LINE"):
    env.pop(key, None)


def run(cwd, *args, success=True, command_env=None):
    result = subprocess.run([binary, *map(str, args)], cwd=cwd, env=command_env or env,
                            capture_output=True, text=True, timeout=30)
    assert (result.returncode == 0) == success, (args, result.returncode, result.stdout, result.stderr)
    return result


def project(name):
    source = root / (name + "-source")
    source.mkdir()
    (source / "file").write_text("base\n")
    (source / ".gitignore").write_text("target/\n")
    (source / ".layrattributes").write_text("target regenerable\n")
    run(root, "init", name, "--from", source, "-q")
    main = root / "store/projects" / name / "lines/main"
    run(main, "line", "new", "-q", "topic")
    return main.parent / "topic"


def peer_permissions(action):
    work = project("permission-" + action)
    user = pwd.getpwuid(os.getuid()).pw_name
    run(work, "remote", "add", "peer", "ssh://test/permission")
    bare = root / ("permission-" + action + ".git")
    subprocess.run(["git", "init", "--bare", "-q", str(bare)], check=True)
    run(work, "git", "remote", "add", "origin", bare)
    marker = root / ("transport-started-" + action)
    run(work, "access", "deny", user, action)
    run(work, "git", "push", "-q", "origin", "topic")
    refused = run(work, "push", "peer", "--command", "--", "sh", "-c",
                  'touch "$1"', "sh", marker, success=False)
    assert "permission denied" in refused.stderr and action in refused.stderr, refused.stderr
    assert not marker.exists(), "the peer transport started before the permission check"


def symlink_root():
    # The alias is outside the store. Existing path safety inside lines stays active.
    alias = root / "alias"
    alias.symlink_to(root, target_is_directory=True)
    alias_env = dict(env, LAYR_ROOT=str(alias / "store"))
    source = root / "alias-source"
    source.mkdir()
    (source / "file").write_text("alias\n")
    run(root, "init", "alias", "--from", source, "-q", command_env=alias_env)
    run(root, "--project", "alias", "--line", "main", "line", "new", "-q", "topic", command_env=alias_env)
    work = root / "store/projects/alias/lines/topic"
    (work / "file").write_text("saved through an alias\n")
    run(work, "save", "-qam", "alias", command_env=alias_env)
    assert run(work, "show", "head:file", command_env=alias_env).stdout == "saved through an alias\n"


def lock_is_open(proc, lock):
    if sys.platform == "darwin":
        result = subprocess.run(["/usr/sbin/lsof", "-a", "-p", str(proc.pid), "-Fn", str(lock)],
                                capture_output=True, text=True, timeout=5)
        return "n" + str(lock) in result.stdout.splitlines()
    for fd in Path(f"/proc/{proc.pid}/fd").iterdir():
        try:
            if fd.resolve() == lock:
                return True
        except FileNotFoundError:
            pass
    return False


def merge_source_moves():
    work = project("merge-race")
    run(work, "line", "new", "-q", "destination")
    destination = work.parent / "destination"
    (work / "file").write_text("reviewed\n")
    run(work, "save", "-qam", "reviewed")
    reviewed = run(work, "rev-parse", "head").stdout.strip()
    (work / "file").write_text("later\n")
    run(work, "save", "-qam", "later")
    later = run(work, "rev-parse", "head").stdout.strip()
    run(work, "reset", "--hard", "-q", reviewed)
    before = run(destination, "rev-parse", "head").stdout.strip()
    lock = work.parents[1] / "meta/lock"
    with lock.open("w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        proc = subprocess.Popen([binary, "merge", "topic", "--expect", reviewed], cwd=destination,
                                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 20
            while not lock_is_open(proc, lock):
                assert proc.poll() is None, proc.communicate()
                assert time.monotonic() < deadline, "merge did not reach the project lock"
                time.sleep(0.01)
            # Inject the derived heads of a concurrent source reset while the lock is held.
            # Both states already exist. Restore the fixture before checking signed history.
            database = work.parents[1] / "meta/db/layr.sqlite"
            def set_source(head):
                with sqlite3.connect(database) as connection:
                    state_id = connection.execute("SELECT id FROM states WHERE display=?", (head,)).fetchone()[0]
                    connection.execute("UPDATE lines SET head=? WHERE name='topic'", (state_id,))
                    connection.execute("UPDATE line_heads SET head=? WHERE line=(SELECT id FROM lines WHERE name='topic')", (state_id,))
            set_source(later)
            try:
                fcntl.flock(held, fcntl.LOCK_UN)
                stdout, stderr = proc.communicate(timeout=30)
            finally:
                set_source(reviewed)
            run(work, "reset", "--hard", "-q", later)
            assert proc.returncode != 0 and "--expect" in stderr, (proc.returncode, stdout, stderr)
            assert run(destination, "rev-parse", "head").stdout.strip() == before
            assert (destination / "file").read_text() == "base\n"
            # Exact state IDs remain valid after their source line moves.
            run(destination, "merge", reviewed, "--expect", reviewed, "-q")
            assert (destination / "file").read_text() == "reviewed\n"
            run(destination, "fsck")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.communicate(timeout=10)


def cache_conversion(repair, check_links):
    work = project("cache-" + ("repair" if repair else "first") + ("-links" if check_links else "-times"))
    target = work / "target"
    if repair:
        target.mkdir()
        run(work, "save", "--caches", "-q")
        target.rmdir()
    (target / "nested").mkdir(parents=True)
    (target / "a").write_bytes(b"cache data\n" * 2048)
    os.link(target / "a", target / "nested/b")
    os.utime(target / "a", ns=(1700000000123456789, 1700000000123456789))
    run(work, *( ["overlay", "repair", "cache.target"] if repair else ["save", "--caches", "-q"] ))
    a, b = (target / "a").stat(), (target / "nested/b").stat()
    assert (target / "a").read_bytes() == b"cache data\n" * 2048
    if check_links:
        assert a.st_ino == b.st_ino, (a.st_ino, b.st_ino)
    else:
        assert a.st_mtime_ns == 1700000000123456789, a.st_mtime_ns


def pnpm_cache(repair):
    work = project("pnpm-" + str(repair).lower())
    target = work / "target"
    if repair:
        target.mkdir()
        run(work, "save", "--caches", "-q")
        target.rmdir()
    package = target / ".pnpm/pkg@1/node_modules/pkg"
    package.mkdir(parents=True)
    (package / "index.js").write_text("package\n")
    (target / "pkg").symlink_to(".pnpm/pkg@1/node_modules/pkg")
    nested = target / ".pnpm/tool@1/node_modules"
    nested.mkdir(parents=True)
    (nested / "pkg").symlink_to("../../pkg@1/node_modules/pkg")
    (target / ".bin").mkdir()
    shim = target / ".bin/pkg"
    shim.write_text('#!/bin/sh\ncat "$(dirname "$0")/../pkg/index.js"\n')
    shim.chmod(0o755)
    (target / ".bin/pkg-link").symlink_to("../pkg/index.js")
    (target / "dangling").symlink_to("missing")
    run(work, *(["overlay", "repair", "cache.target"] if repair else ["save", "--caches", "-q"]))
    for path, expected in [("pkg", ".pnpm/pkg@1/node_modules/pkg"),
                           (".pnpm/tool@1/node_modules/pkg", "../../pkg@1/node_modules/pkg"),
                           (".bin/pkg-link", "../pkg/index.js"), ("dangling", "missing")]:
        assert os.readlink(target / path) == expected, path
    assert (nested / "pkg/index.js").read_text() == "package\n"
    assert subprocess.check_output([str(shim)], text=True) == "package\n"


def failed_cache_copy(repair, unreadable=False):
    work = project("cache-failure-" + str(repair).lower() + ("-unreadable" if unreadable else "-special"))
    target = work / "target"
    target.mkdir()
    if repair:
        run(work, "save", "--caches", "-q")
        target.rmdir()
        target.mkdir()
    (target / "kept").write_text("keep this\n")
    if unreadable:
        (target / "unreadable").write_text("private output\n")
        (target / "unreadable").chmod(0)
    else:
        os.mkfifo(target / "unsupported")
    try:
        run(work, *(["overlay", "repair", "cache.target"] if repair else ["save", "--caches", "-q"]), success=False)
    finally:
        if unreadable and (target / "unreadable").exists():
            (target / "unreadable").chmod(0o600)
    assert (target / "kept").read_text() == "keep this\n"
    assert not list(work.glob("target.layr-*")), "failed conversion leaves a temporary copy"
    if unreadable:
        assert (target / "unreadable").read_text() == "private output\n"
    else:
        import stat
        assert stat.S_ISFIFO((target / "unsupported").lstat().st_mode)



def cache_readonly(repair):
    work = project("cache-readonly-" + str(repair).lower())
    target = work / "target"
    target.mkdir()
    if repair:
        run(work, "save", "--caches", "-q")
        target.rmdir()
        target.mkdir()
    nested = target / "nested"
    nested.mkdir()
    data = nested / "data"
    data.write_text("read-only cache output\n")
    data.chmod(0o444)
    nested.chmod(0o555)
    target.chmod(0o555)
    try:
        run(work, *(["overlay", "repair", "cache.target"] if repair else ["save", "--caches", "-q"]))
        assert data.read_text() == "read-only cache output\n"
        assert target.stat().st_mode & 0o777 == 0o555
        assert nested.stat().st_mode & 0o777 == 0o555
        assert data.stat().st_mode & 0o777 == 0o444
        assert not list(work.glob("target.layr-*"))
    finally:
        # Release only this fixture's directory modes so the test harness can remove it.
        for folder in (target, *work.glob("target.layr-*")):
            if folder.exists():
                for base, directories, files in os.walk(folder):
                    os.chmod(base, os.stat(base).st_mode | 0o700)


def cache_directory_metadata(repair, kind):
    work = project("cache-metadata-" + str(repair).lower() + "-" + kind)
    target = work / "target"
    target.mkdir()
    if repair:
        run(work, "save", "--caches", "-q")
        target.rmdir()
        target.mkdir()
    nested = target / "nested"
    nested.mkdir()
    data = nested / "data"
    data.write_text("output\n")
    target.chmod(0o700)
    nested.chmod(0o750)
    attribute = "com.layr.test" if sys.platform == "darwin" else "user.layr.test"
    for path in (target, nested):
        if sys.platform == "darwin":
            subprocess.run(["/usr/bin/xattr", "-w", attribute, "cache directory", str(path)], check=True)
        else:
            os.setxattr(path, attribute, b"cache directory")
    if kind == "acl":
        for path in (target, nested, data):
            subprocess.run(["/bin/chmod", "+a", "everyone allow read,readattr,readextattr,readsecurity", str(path)], check=True)
        def acl(path):
            return subprocess.check_output(["/bin/ls", "-lde", str(path)], text=True).splitlines()[1:]
        expected = [acl(path) for path in (target, nested, data)]
        assert all(expected), expected
    run(work, *(["overlay", "repair", "cache.target"] if repair else ["save", "--caches", "-q"]))
    if kind == "mode":
        assert target.stat().st_mode & 0o777 == 0o700, oct(target.stat().st_mode)
        assert nested.stat().st_mode & 0o777 == 0o750, oct(nested.stat().st_mode)
    elif kind == "xattr":
        for path in (target, nested):
            if sys.platform == "darwin":
                assert subprocess.check_output(["/usr/bin/xattr", "-p", attribute, str(path)]).strip() == b"cache directory"
            else:
                assert os.getxattr(path, attribute) == b"cache directory"
    else:
        assert [acl(path) for path in (target, nested, data)] == expected
    assert "\tcache\t" in run(work, "overlay", "list").stdout


def cache_symlink_xattrs(repair):
    work = project("cache-link-xattr-" + str(repair).lower())
    target = work / "target"
    target.mkdir()
    if repair:
        run(work, "save", "--caches", "-q")
        target.rmdir()
        target.mkdir()
    outside = root / ("link-referent-" + str(repair).lower())
    outside.write_text("outside the cache\n")
    (target / "link").symlink_to(outside)
    (target / "dangling").symlink_to("missing")
    attribute = "com.layr.test" if sys.platform == "darwin" else "trusted.layr.test"
    def put(path, value):
        if sys.platform == "darwin":
            subprocess.run(["/usr/bin/xattr", "-s", "-w", attribute, value, str(path)], check=True)
        else:
            os.setxattr(path, attribute, value.encode(), follow_symlinks=False)
    def get(path):
        if sys.platform == "darwin":
            return subprocess.check_output(["/usr/bin/xattr", "-s", "-p", attribute, str(path)]).strip()
        return os.getxattr(path, attribute, follow_symlinks=False)
    put(outside, "referent attribute")
    put(target / "link", "link attribute")
    put(target / "dangling", "dangling attribute")
    run(work, *(["overlay", "repair", "cache.target"] if repair else ["save", "--caches", "-q"]))
    assert get(target / "link") == b"link attribute"
    assert get(target / "dangling") == b"dangling attribute"
    assert get(outside) == b"referent attribute"
    assert outside.read_text() == "outside the cache\n"


def store_permissions(op):
    work = project("store-permission-" + op)
    archive = root / ("archive-" + op)
    run(work, "remote", "add", "archive", "file://" + str(archive), "--paper-key-out", root / (op + ".paper"))
    run(work, "push", "archive", "-q")
    user = pwd.getpwuid(os.getuid()).pw_name
    run(work, "access", "deny", user, "records")
    if op == "push":
        (work / "file").write_text("must stay local\n")
        run(work, "save", "-qam", "local")
    result = run(work, op, "archive", "-q", success=False)
    assert "permission denied" in result.stderr and "records" in result.stderr, result.stderr


def merge_policy_changes(kind):
    work = project("merge-policy-" + kind)
    main = work.parent / "main"
    run(main, "protect", "main", "--no-direct", "--check", "wait")
    started = root / (kind + "-started")
    release = root / (kind + "-release")
    command = f"touch {shlex.quote(str(started))}; while ! test -f {shlex.quote(str(release))}; do sleep 0.05; done"
    run(main, "config", "check.wait", command)
    (work / "file").write_text("reviewed\n")
    run(work, "save", "-qam", "reviewed")
    reviewed = run(work, "rev-parse", "head").stdout.strip()
    before = run(main, "rev-parse", "head").stdout.strip()
    proc = subprocess.Popen([binary, "merge", "topic", "--expect", reviewed, "-q"], cwd=main,
                            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.monotonic() + 20
        while not started.exists():
            assert proc.poll() is None, proc.communicate()
            assert time.monotonic() < deadline, "merge did not start the configured check"
            time.sleep(0.01)
        if kind == "permission":
            run(work, "access", "deny", pwd.getpwuid(os.getuid()).pw_name, "merge")
        elif kind == "protection":
            run(work, "protect", "main", "--approvals", "1")
        else:
            run(work, "config", "check.wait", "false")
        release.touch()
        stdout, stderr = proc.communicate(timeout=30)
        assert proc.returncode != 0, (stdout, stderr)
        assert run(main, "rev-parse", "head").stdout.strip() == before
        assert (main / "file").read_text() == "base\n"
    finally:
        release.touch()
        if proc.poll() is None:
            proc.kill()
            proc.communicate(timeout=10)


cases = [("store root through a symbolic link supports init and save", symlink_root),
         ("merge refuses a source that moves while it waits for the project lock", merge_source_moves)]
# Root bypasses policy. The Linux role case runs in t13-access.sh.
if os.getuid() != 0:
    for action in ("sync", "records"):
        cases.append((f"peer push requires {action}; Git push remains allowed",
                      lambda action=action: peer_permissions(action)))
    for op in ("push", "fetch"):
        cases.append((f"store {op} requires records", lambda op=op: store_permissions(op)))
    cases.append(("merge refuses permission revoked during checks", lambda: merge_policy_changes("permission")))
for kind in ("protection", "command"):
    cases.append((f"merge refuses {kind} changed during checks", lambda kind=kind: merge_policy_changes(kind)))
for repair in (False, True):
    cases.append((f"read-only cache conversion keeps modes and removes old trees (repair={repair})", lambda repair=repair: cache_readonly(repair)))
    cases.append((f"pnpm cache preserves symlinks and shims (repair={repair})", lambda repair=repair: pnpm_cache(repair)))
    cases.append((f"cache preserves symlink xattrs (repair={repair})", lambda repair=repair: cache_symlink_xattrs(repair)))
    cases.append((f"failed cache copy preserves the source (repair={repair})", lambda repair=repair: failed_cache_copy(repair)))
    if os.getuid() != 0:
        cases.append((f"unreadable cache entry preserves the source (repair={repair})",
                      lambda repair=repair: failed_cache_copy(repair, unreadable=True)))
    for kind in (("mode", "xattr", "acl") if sys.platform == "darwin" else ("mode", "xattr")):
        cases.append((f"cache preserves directory {kind} (repair={repair})",
                      lambda repair=repair, kind=kind: cache_directory_metadata(repair, kind)))
    for links in (False, True):
        cases.append((f"cache {'repair' if repair else 'first conversion'} preserves {'hard links' if links else 'nanoseconds'}",
                      lambda repair=repair, links=links: cache_conversion(repair, links)))
selected = [(name, case) for name, case in cases if len(sys.argv) < 4 or sys.argv[3] in name]
failures = 0
for name, case in selected:
    try:
        case()
        print("PASS " + name, flush=True)
    except Exception as error:
        failures += 1
        print(f"FAIL {name}: {error!r}", flush=True)
print(f"t50-upstream-fixes.sh: {len(selected) - failures} passed, {failures} failed")
sys.exit(bool(failures))
