#!/usr/bin/env python3
"""Layr with a store on an APFS volume that ignores case (the macOS default).

The test makes and removes two small images: a case-sensitive one for sources that hold names
that differ only in case, and a case-insensitive one for the store.
"""
import argparse, os, shutil, subprocess, tempfile
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--binary', required=True)
p.add_argument('--root', required=True, help='an empty folder for the images (any volume)')
a = p.parse_args()
binary = str(Path(a.binary).resolve())
base = Path(tempfile.mkdtemp(prefix='apfs-case-', dir=Path(a.root).resolve()))
mounts = []


def image(name, fs):
    img, mnt = base / f'{name}.sparseimage', base / name
    mnt.mkdir()
    subprocess.run(['hdiutil', 'create', '-size', '1g', '-type', 'SPARSE', '-fs', fs, '-volname', name, str(img)],
                   check=True, capture_output=True)
    subprocess.run(['hdiutil', 'attach', '-nobrowse', '-mountpoint', str(mnt), str(img)], check=True, capture_output=True)
    mounts.append(mnt)
    return mnt


env = dict(os.environ, LAYR_AUTHOR_NAME='Test', LAYR_AUTHOR_EMAIL='test@example.invalid')
for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE', 'LAYR_TRACE', 'LAYR_ROOT'):
    env.pop(key, None)


def run(cwd, store, *args, ok=True):
    r = subprocess.run([binary, *map(str, args)], cwd=cwd, env=dict(env, LAYR_ROOT=str(store)), capture_output=True, text=True)
    assert (r.returncode == 0) == ok, (args, r.returncode, r.stdout, r.stderr)
    return r


def names(folder):
    return sorted(os.listdir(folder))


try:
    cs = image('Sensitive', 'Case-sensitive APFS')
    ci = image('Insensitive', 'APFS')
    probe = ci / 'Probe'
    probe.write_text('x')
    assert (ci / 'probe').exists(), 'the store image must ignore case'
    probe.unlink()

    src = cs / 'src'
    src.mkdir()
    (src / 'README').write_text('upper\n')
    (src / 'Readme').write_text('lower\n')
    (src / 'other').write_text('other\n')

    # 1. A source with names that differ only in case is refused, and nothing is half made.
    store = ci / 'store'
    r = run(ci, store, 'init', 'p', '--from', src, '-q', ok=False)
    assert 'differ only in case' in r.stderr and 'README, Readme' in r.stderr, r.stderr
    print('PASS init refuses README and Readme', flush=True)

    # 2. A git commit with such names is refused.
    repo = cs / 'repo'
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    for f in src.iterdir():
        shutil.copy(f, repo / f.name)
    subprocess.run(['git', '-C', str(repo), 'add', '-A'], check=True)
    subprocess.run(['git', '-C', str(repo), '-c', 'user.name=T', '-c', 'user.email=t@x', 'commit', '-qm', 'both'], check=True)
    r = run(ci, ci / 'store-git', 'git', 'clone', repo, 'q', '-q', ok=False)
    assert 'differ only in case' in r.stderr, r.stderr
    print('PASS git clone refuses README and Readme', flush=True)

    # 3. A peer that sends such names is refused.
    peer = cs / 'store-peer'
    run(cs, peer, 'init', 'p', '--from', src, '-q')
    peer_main = peer / 'projects/p/lines/main'
    run(peer_main, peer, 'remote', 'add', 'other', 'ssh://test/p')
    r = run(peer_main, peer, 'sync', 'other', '--command', '--', 'env', f'LAYR_ROOT={ci / "store-sync"}', binary, 'serve-peer', 'p', ok=False)
    assert 'differ only in case' in r.stderr, r.stderr
    print('PASS a peer sync refuses README and Readme', flush=True)

    # 4. A normal project works; renames that change only case keep the new names.
    plain = ci / 'plain'
    (plain / 'dir').mkdir(parents=True)
    (plain / 'readme').write_text('text\n')
    (plain / 'dir/a').write_text('a\n')
    (plain / 'Makefile').write_text('all:\n')
    run(ci, store, 'init', 'p', '--from', plain, '-q')
    main = store / 'projects/p/lines/main'
    run(main, store, 'protect', 'main', '--remove')
    run(main, store, 'branch', '-q', 't')
    work = main.parent / 't'
    os.rename(work / 'readme', work / 'README')
    os.rename(work / 'dir', work / 'Dir')
    run(work, store, 'add', '-A')
    status = run(work, store, 'status', '--porcelain').stdout
    assert 'R  readme -> README' in status and 'R  dir/a -> Dir/a' in status, status
    run(work, store, 'commit', '-qm', 'case only')
    assert not run(work, store, 'status', '--porcelain').stdout
    assert run(work, store, 'ls-files').stdout.split() == ['Dir/a', 'Makefile', 'README']
    run(work, store, 'branch', '-q', 't2')
    assert names(main.parent / 't2') == ['.git', 'Dir', 'Makefile', 'README'] or names(main.parent / 't2') == ['Dir', 'Makefile', 'README']
    run(main, store, 'merge', '-q', 't')
    assert 'README' in names(main) and 'readme' not in names(main) and 'Dir' in names(main)
    assert not run(main, store, 'status', '--porcelain').stdout
    print('PASS a rename that changes only case keeps the new name', flush=True)

    # 5. mv with a change of case uses the complete index.
    run(work, store, 'mv', 'Makefile', 'makefile')
    status = run(work, store, 'status', '--porcelain').stdout
    assert 'R  Makefile -> makefile' in status, status
    run(work, store, 'commit', '-qm', 'mv case')
    assert 'makefile' in names(work) and 'Makefile' not in names(work)
    assert 'makefile' in run(work, store, 'ls-files').stdout.split()
    print('PASS mv with a change of case', flush=True)

    # 6. A folder staged with one case, then renamed back before the commit.
    (work / 'Dir/b').write_text('b\n')
    run(work, store, 'add', '-A')
    os.rename(work / 'Dir', work / 'dir.tmp')
    os.rename(work / 'dir.tmp', work / 'dir')
    run(work, store, 'add', '-A')
    run(work, store, 'commit', '-qm', 'folder back to lower case')
    files = run(work, store, 'ls-files').stdout.split()
    assert 'dir/a' in files and 'dir/b' in files and not any(f.startswith('Dir/') for f in files), files
    assert not run(work, store, 'status', '--porcelain').stdout
    print('PASS a folder staged with one case and renamed back', flush=True)

    # 7. reset --hard to a state with the other case of a name.
    run(work, store, 'reset', '-q', '--hard', 'HEAD~1')
    assert 'Dir' in names(work) and 'dir' not in names(work), names(work)
    assert not run(work, store, 'status', '--porcelain').stdout
    print('PASS reset --hard across a change of case', flush=True)

    # 8. Hard links on a volume that ignores case.
    os.link(work / 'README', work / 'second')
    run(work, store, 'add', '-A')
    run(work, store, 'commit', '-qm', 'second name')
    assert not run(work, store, 'status', '--porcelain').stdout
    assert 'second' in run(work, store, 'ls-files').stdout.split()
    (work / 'second').unlink()
    run(work, store, 'add', '-A')
    run(work, store, 'commit', '-qm', 'one name')
    assert 'README' in names(work) and 'README' in run(work, store, 'ls-files').stdout.split()
    print('PASS hard links', flush=True)

    # 9. Two lines that add NOTES and notes: the merge refuses and changes nothing.
    run(main, store, 'branch', '-q', 'u')
    (main / 'NOTES').write_text('upper\n')
    run(main, store, 'add', '-A')
    run(main, store, 'commit', '-qm', 'upper')
    head = run(main, store, 'rev-parse', 'HEAD').stdout
    u = main.parent / 'u'
    (u / 'notes').write_text('lower\n')
    run(u, store, 'add', '-A')
    run(u, store, 'commit', '-qm', 'lower')
    r = run(main, store, 'merge', 'u', ok=False)
    assert 'differ only in case' in r.stderr, r.stdout + r.stderr
    assert (main / 'NOTES').read_text() == 'upper\n' and names(main).count('NOTES') == 1 and 'notes' not in names(main)
    assert run(main, store, 'rev-parse', 'HEAD').stdout == head
    assert not run(main, store, 'status', '--porcelain').stdout
    print('PASS a merge of NOTES and notes refuses and changes nothing', flush=True)

    run(work, store, 'fsck')
    run(main, store, 'fsck')
    print('PASS fsck', flush=True)
finally:
    for m in mounts:
        subprocess.run(['hdiutil', 'detach', '-force', str(m)], capture_output=True)
    shutil.rmtree(base, ignore_errors=True)
