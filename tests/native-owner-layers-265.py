#!/usr/bin/env python3
# ci: overlayfs
"""Companion for the lead's perf/itest-root.sh isolated native root controller.

The harness must copy this file and the binary before hiding host homes/stores,
then call run(binary, empty_root_owned_case_dir). All Layr clients run as uid 1000;
root only starts the daemon, makes adversarial fixtures and reads private metadata.
The controller owns cleanup after stopping all namespace processes. Never run on
an ordinary host shell. --dry exercises fixture/assertion helpers as uid 1000.
"""
import argparse
import json
import os
from pathlib import Path
import pwd
import stat
import subprocess
import tempfile
import time

UID = 1000
GID = pwd.getpwuid(UID).pw_gid
OTHER = 65534
OTHER_GID = pwd.getpwuid(OTHER).pw_gid


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def entries(root):
    """Never follow symlinks or descend into nested mounts/managed roots."""
    yield root
    device = root.stat().st_dev
    for directory, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = [name for name in dirs if name != '.git' and
                   not Path(directory, name).is_mount() and
                   Path(directory, name).lstat().st_dev == device]
        for name in dirs + files:
            yield Path(directory, name)


def owned(root, uid, gid):
    for path in entries(root):
        meta = path.lstat()
        check((meta.st_uid, meta.st_gid) == (uid, gid), f'wrong owner: {path}: {meta.st_uid}:{meta.st_gid}')


def contents(root):
    check((root / 'files/00099').read_text() == 'file 99\n', 'unchanged content')
    check((root / 'files/00000').read_text() == 'edited 0\n', 'staged content')
    check(os.readlink(root / 'link') == 'files/00099', 'symlink target')
    check((root / 'files/00099').stat().st_ino == (root / 'twin').stat().st_ino, 'hard-link group')


def bounded_upper(store, line):
    home = store / '.layr-overlayfs'
    registry = json.loads((home / 'registry.json').read_text())['trees']
    entry = registry[str(line)]
    path = home / 'objects' / entry['info']['uuid'] / 'upper'
    check(not entry['hard_links'], 'sparse case unexpectedly selected hard-link full-copy fallback')
    paths = list(path.rglob('*')) if path.exists() else []
    files = [p for p in paths if stat.S_ISREG(p.lstat().st_mode)]
    # A root inode / a few bookkeeping directories may copy up; no inherited payload may.
    check(len(paths) <= 32 and not files, f'new upper contains {len(paths)} entries / {len(files)} files')
    print(json.dumps(dict(line=line.name, upper=str(path), entries=len(paths), files=len(files))))


def run(binary, case):
    """Called only by the lead's root controller inside its private PID/mount namespace."""
    check(os.geteuid() == 0, 'root controller required; clients remain uid 1000')
    init = Path('/proc/1/cmdline').read_bytes()
    check(b'--inside' in init and (b'itest-root.sh' in init or b'root-measure' in init), 'lead isolation harness required')
    check(not Path('/home/alex/src/layr').exists(), 'host home must be hidden')
    binary, case = Path(binary).resolve(strict=True), Path(case).resolve(strict=True)
    check(case.stat().st_uid == 0 and not list(case.iterdir()), 'empty root-owned case directory required')
    os.chmod(case, 0o711)
    work = case / 'client'
    work.mkdir(mode=0o700)
    os.chown(work, UID, GID)
    store, sock = case / 'store', case / 'service.sock'
    env = dict(PATH='/usr/bin:/bin', HOME=str(work), TMPDIR=str(work), LAYR_ROOT=str(store),
               LAYR_SOCKET=str(sock), LAYR_ALLOW_ROOT_SNAPSHOTS='1',
               LAYR_AUTHOR_NAME='Owner regression', LAYR_AUTHOR_EMAIL='owner@example.invalid')
    prefix = ['setpriv', f'--reuid={UID}', f'--regid={GID}', '--clear-groups', '--no-new-privs',
              '--inh-caps=-all', '--ambient-caps=-all', '--']

    def client(*args, cwd=work):
        result = subprocess.run([*prefix, str(binary), *args], cwd=cwd, env=env, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
        check(result.returncode == 0,
              f'client {args}: {result.returncode}\n{result.stdout}\n{result.stderr}')
        return result.stdout

    source = work / 'source'
    (source / 'files').mkdir(parents=True)
    for i in range(1000):
        (source / f'files/{i:05}').write_text(f'file {i}\n')
    (source / 'link').symlink_to('files/00099')
    outside = case / 'outside'
    outside.write_text('outside\n')
    (source / 'escape').symlink_to(outside)
    # Readable mixed source owners must all become the intended owner, never retained root.
    for path in entries(source):
        os.chown(path, UID, GID, follow_symlinks=False)
    os.chown(source / 'files/00099', 0, 0)
    (source / '.gitignore').write_text('target/\n')
    (source / '.layrattributes').write_text('target regenerable\n')
    for path in [source / '.gitignore', source / '.layrattributes']:
        os.chown(path, UID, GID)
    outside_before = outside.stat()
    with (case / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(binary), 'daemon', '--socket', str(sock), '--tick', '3600'],
                                  cwd=case, env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            for _ in range(100):
                check(daemon.poll() is None, 'daemon exited; inspect daemon.log')
                if sock.exists():
                    break
                time.sleep(0.05)
            check(sock.exists(), 'daemon socket did not appear')
            client('init', 'owners', '--from', str(source), '-q')
            main = store / 'projects/owners/lines/main'
            client('protect', 'main', '--remove', cwd=main)
            owned(main, UID, GID)
            # An ignored nested cache is copied separately, never walked as base content.
            cache = main / 'target/debug/cache'
            cache.parent.mkdir(parents=True, exist_ok=True)
            cache.write_text('private cache\n')
            for path in [main / 'target', cache.parent, cache]:
                os.chown(path, UID, GID)
            client('save', '--overlays', '-q', cwd=main)
            times = (main / 'target/.layr-times').lstat()
            check((times.st_uid, times.st_gid) == (UID, GID), 'recorded cache times belong to the service')
            # Ten-file stage/save must preserve every unchanged file's owner.
            for i in range(10):
                path = main / f'files/{i:05}'
                path.write_text(f'edited {i}\n')
            client('add', *[f'files/{i:05}' for i in range(10)], cwd=main)
            client('save', '-qm', 'ten files', cwd=main)
            client('line', 'new', 'same', 'main', '-q', cwd=main)
            same = main.parent / 'same'
            owned(same, UID, GID)
            check((same / 'target/debug/cache').read_text() == 'private cache\n', 'nested cache not inherited')
            check((same / 'files/00099').read_text() == 'file 99\n', 'unchanged bytes')
            check((same / 'files/00000').read_text() == 'edited 0\n', 'staged bytes')
            if (store / '.layr-overlayfs').exists():
                bounded_upper(store, same)
            # Another owner must receive every entry, including a link, without altering the seed.
            client('line', 'new', 'other', 'main', '--owner', str(OTHER), '-q', cwd=main)
            other = main.parent / 'other'
            owned(other, OTHER, OTHER_GID)
            check(stat.S_IMODE(other.stat().st_mode) == 0o700, 'another owner line is not private')
            denied = subprocess.run([*prefix, 'cat', str(other / 'files/00099')], env=env,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
            check(denied.returncode != 0 and b'Permission denied' in denied.stderr,
                  'uid 1000 can read the other owner private line')
            owned(main, UID, GID)
            check(cache.read_text() == 'private cache\n', 'another owner modified the source cache')
            # Same root owner with mixed descendants: force an automatic state retaining metadata.
            os.chown(main / 'files/00099', 0, 0)
            os.chown(main / 'link', 0, 0, follow_symlinks=False)
            mixed = json.loads(client('save', '--force', '--json', cwd=main))['state']
            client('line', 'new', 'mixed', mixed, '-q', cwd=main)
            owned(main.parent / 'mixed', UID, GID)
            check((main.parent / 'mixed/escape').is_symlink(), 'external symlink was replaced')
            check((main / 'files/00099').stat().st_uid == 0, 'immutable/source owner was mutated')
            # Hard links use their own backend path; keep topology and ownership correct.
            os.link(source / 'files/00099', source / 'twin')
            client('init', 'linked', '--from', str(source), '-q')
            linked = store / 'projects/linked/lines/main'
            client('protect', 'main', '--remove', cwd=linked)
            for i in range(10):
                (linked / f'files/{i:05}').write_text(f'edited {i}\n')
            client('add', *[f'files/{i:05}' for i in range(10)], cwd=linked)
            client('save', '-qm', 'linked ten files', cwd=linked)
            client('line', 'new', 'same', 'main', '-q', cwd=linked)
            contents(linked.parent / 'same')
            owned(linked.parent / 'same', UID, GID)
            client('migrate', '--delete-old', cwd=linked)
            client('line', 'new', 'objects', 'main', '-q', cwd=linked)
            contents(linked.parent / 'objects')
            owned(linked.parent / 'objects', UID, GID)
            after = outside.stat()
            check((after.st_uid, after.st_gid, after.st_ctime_ns) ==
                  (outside_before.st_uid, outside_before.st_gid, outside_before.st_ctime_ns), 'symlink escaped import/preparation')
            print('owner layers native regression: PASS')
        finally:
            daemon.terminate()
            daemon.wait(timeout=15)
    # The harness stops/reaps all remaining request processes and removes managed roots.


def dry():
    check(os.geteuid() == UID, '--dry must run as uid 1000')
    with tempfile.TemporaryDirectory(dir=os.environ['TMPDIR']) as temp:
        root = Path(temp)
        (root / 'files').mkdir()
        (root / 'files/00099').write_text('file 99\n')
        (root / 'files/00000').write_text('edited 0\n')
        (root / 'link').symlink_to('files/00099')
        os.link(root / 'files/00099', root / 'twin')
        contents(root)
        owned(root, UID, GID)
        store = root / 'store'
        home = store / '.layr-overlayfs'
        path = home / 'objects/test/upper'
        path.mkdir(parents=True)
        line = root / 'line'
        (home / 'registry.json').write_text(json.dumps(dict(trees={str(line): dict(info=dict(uuid='test'), hard_links=False)})))
        bounded_upper(store, line)
        (path / 'unexpected').write_text('copy-up')
        try:
            bounded_upper(store, line)
        except AssertionError:
            pass
        else:
            raise AssertionError('upper assertion accepted a copied-up file')
    print('owner layers dry assertions: PASS (no daemon or privileged operations)')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dry', action='store_true')
    parser.add_argument('--run', nargs=2, metavar=('BINARY', 'EMPTY_CASE_DIR'))
    args = parser.parse_args()
    if args.dry and not args.run:
        dry()
    elif args.run and not args.dry:
        run(*args.run)
    else:
        parser.error('choose --dry or --run')
