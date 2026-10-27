#!/usr/bin/env python3
"""Measure isolated filesystem space for source and warm-cache workspaces.

Run only inside an isolated Linux VM/container with no host bind mounts.
Each case creates a new loop image, retains it, and unmounts only that image.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--root', required=True)
p.add_argument('--btrfs-binary', required=True)
p.add_argument('--overlay-binary', required=True)
p.add_argument('--branches', type=int, default=16)
p.add_argument('--files', type=int, default=2048)
p.add_argument('--cache-mib', type=int, default=128)
a = p.parse_args()
root = Path(a.root).resolve()
root.mkdir(parents=True, exist_ok=False)
commands = []

def run(args, cwd=None, env=None):
    r = subprocess.run(list(map(str, args)), cwd=cwd, env=env, text=True, capture_output=True)
    commands.append({'argv': list(map(str, args)), 'exit': r.returncode})
    if r.returncode:
        raise RuntimeError(f'{args}: {r.stdout}\n{r.stderr}')
    return r.stdout.strip()

source = root / 'fixture'
source.mkdir()
rng = random.Random(20261016)
digest = hashlib.sha256()
for i in range(a.files):
    # Actual allocated bytes, deterministic text with distinct file content.
    data = ''.join(f'{rng.getrandbits(64):016x} source line {i}\n' for _ in range(300)).encode()[:8192]
    (source / f'f{i:05}.txt').write_bytes(data)
    digest.update(data)
(source / '.gitignore').write_text('target/\n')
cache = root / 'cache-fixture'
cache.mkdir()
cache_hash = hashlib.sha256()
for i in range(a.cache_mib):
    data = rng.randbytes(1024 * 1024)
    (cache / f'object{i:04}').write_bytes(data)
    cache_hash.update(data)
report = {'platform': platform.platform(), 'branches': a.branches,
          'source_files': a.files, 'source_bytes': sum(f.stat().st_size for f in source.iterdir()),
          'fixture_sha256': digest.hexdigest(), 'cache_bytes': a.cache_mib * 1024**2,
          'cache_sha256': cache_hash.hexdigest(), 'git_version': run(['git', '--version']),
          'btrfs_version': run(['btrfs', 'version']), 'cases': [], 'commands': commands,
          'binary_sha256': {k: hashlib.sha256(Path(v).read_bytes()).hexdigest()
                            for k, v in [('btrfs', a.btrfs_binary), ('overlayfs', a.overlay_binary)]}}

def save():
    (root / 'results.json').write_text(json.dumps(report, indent=2) + '\n')

def used(mount, fs):
    os.sync()
    if fs == 'btrfs':
        run(['btrfs', 'filesystem', 'sync', mount])
        raw = run(['btrfs', 'filesystem', 'usage', '-b', mount])
        # Actual used extents, including metadata and redundancy. Not chunk allocation.
        value = next(line.split(':', 1)[1].strip() for line in raw.splitlines()
                     if line.strip().startswith('Used:'))
        return {'used_bytes': int(value), 'raw': raw}
    st = os.statvfs(mount)
    return {'used_bytes': (st.f_blocks - st.f_bfree) * st.f_frsize,
            'blocks': st.f_blocks, 'free_blocks': st.f_bfree, 'block_size': st.f_frsize}

cases = [('layr', 'btrfs', 'native'), ('layr', 'ext4', 'native'), ('layr', 'xfs', 'native'),
         ('git', 'btrfs', 'never'), ('git', 'ext4', 'never'), ('git', 'xfs', 'never'),
         ('git', 'btrfs', 'always'), ('git', 'xfs', 'always')]
for tool, fs, copy_mode in cases:
    label = f'{tool}-{fs}-{copy_mode}'
    image = root / f'{label}.img'
    with image.open('xb') as f:
        f.truncate(8 * 1024**3)
    mount = root / label
    mount.mkdir()
    mkfs = {'btrfs': ['mkfs.btrfs', '-q', '-f'], 'ext4': ['mkfs.ext4', '-q', '-F', '-E', 'lazy_itable_init=0,lazy_journal_init=0'], 'xfs': ['mkfs.xfs', '-q', '-f']}[fs]
    run(mkfs + [image])
    run(['mount', '-o', 'loop', image, mount])
    row = {'label': label, 'tool': tool, 'filesystem': fs, 'cache_copy': copy_mode,
           'empty': used(mount, fs), 'scenarios': []}
    report['cases'].append(row)
    save()
    try:
        for warm in (False, True):
            name = 'warm' if warm else 'source'
            start = used(mount, fs)
            env = dict(os.environ, LAYR_ROOT=str(mount / 'store'),
                       LAYR_AUTHOR_NAME='Benchmark', LAYR_AUTHOR_EMAIL='bench@example.invalid',
                       GIT_AUTHOR_NAME='Benchmark', GIT_AUTHOR_EMAIL='bench@example.invalid',
                       GIT_COMMITTER_NAME='Benchmark', GIT_COMMITTER_EMAIL='bench@example.invalid')
            for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE'):
                env.pop(key, None)
            binary = a.btrfs_binary if fs == 'btrfs' else a.overlay_binary
            def layr(cwd, *args):
                return run([binary, *args], cwd, env)
            if tool == 'git':
                main = mount / name
                main.mkdir()
                run(['cp', '-a', '--reflink=never', str(source) + '/.', main])
                run(['git', 'init', '-q', '-b', 'main'], main, env)
                run(['git', 'add', '.'], main, env)
                run(['git', 'commit', '-qm', 'base'], main, env)
                run(['git', 'gc', '--prune=now'], main, env)
            else:
                layr(mount, 'init', name, '--from', str(source), '-q')
                main = mount / 'store/projects' / name / 'lines/main'
            if warm:
                if tool == 'layr':
                    layr(main, 'config', 'regenerable', '["target"]')
                else:
                    (main / 'target').mkdir()
                run(['cp', '-a', '--reflink=never', str(cache) + '/.', main / 'target'])
                if tool == 'layr':
                    layr(main, 'save', '-q', '--caches')
            base = used(mount, fs)
            scenario = {'name': name, 'start': start, 'main': base, 'samples': []}
            row['scenarios'].append(scenario)
            for n in range(1, a.branches + 1):
                if tool == 'git':
                    branch = mount / f'{name}-branch-{n}'
                    run(['git', 'worktree', 'add', '-q', '-b', f'topic{n}', branch, 'main'], main, env)
                    if warm:
                        (branch / 'target').mkdir()
                        run(['cp', '-a', f'--reflink={copy_mode}', str(main / 'target') + '/.', branch / 'target'])
                else:
                    layr(main, 'line', 'new', '-q', f'topic{n}')
                    branch = main.parent / f'topic{n}'
                with (branch / 'f00000.txt').open('ab') as f:
                    f.write(f'branch {n}\n'.encode())
                if warm:
                    # Private mutable cache: one 4 KiB change per branch.
                    with (branch / 'target/object0000').open('r+b') as f:
                        f.write(bytes([n % 256]) * 4096)
                    assert (main / 'target/object0000').read_bytes() == (cache / 'object0000').read_bytes()
                    assert (branch / 'target/object0001').read_bytes() == (cache / 'object0001').read_bytes()
                if tool == 'git':
                    run(['git', 'commit', '-qam', f'edit {n}'], branch, env)
                else:
                    layr(branch, 'save', '-qam', f'edit {n}')
                assert (main / 'f00000.txt').read_bytes() == (source / 'f00000.txt').read_bytes()
                if n in {1, 4, 8, a.branches}:
                    sample = used(mount, fs)
                    sample['branches'] = n
                    sample['growth_bytes'] = sample['used_bytes'] - base['used_bytes']
                    sample['scenario_bytes'] = sample['used_bytes'] - start['used_bytes']
                    scenario['samples'].append(sample)
                    save()
                    print(json.dumps({'case': label, 'scenario': name, **sample}), flush=True)
            if tool == 'layr':
                layr(main, 'fsck')
            else:
                run(['git', 'fsck', '--no-progress'], main, env)
            scenario['verified'] = True
            save()
    finally:
        run(['umount', '-R', mount])
save()
