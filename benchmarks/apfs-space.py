#!/usr/bin/env python3
"""Measure isolated APFS container allocation for two Layr binaries."""
import argparse, hashlib, json, os, pathlib, plistlib, subprocess, time

p = argparse.ArgumentParser()
p.add_argument('--before', required=True)
p.add_argument('--after', required=True)
p.add_argument('--root', required=True)
p.add_argument('--repeats', type=int, default=2)
a = p.parse_args()
root = pathlib.Path(a.root).resolve()
root.mkdir(parents=True, exist_ok=False)
report = {'files': 10000, 'small_file_bytes': 4096, 'large_file_bytes': 67108864,
          'measurement': 'Isolated APFS container size minus free space; includes filesystem metadata.',
          'rows': [], 'binaries': {k: hashlib.sha256(pathlib.Path(v).read_bytes()).hexdigest()
                                   for k, v in [('baseline', a.before), ('native', a.after)]}}
def call(args, **kw):
    r = subprocess.run(args, capture_output=True, **kw)
    if r.returncode:
        raise RuntimeError((args, r.returncode, r.stdout, r.stderr))
    return r.stdout

def record(mount, trial, variant, phase):
    samples = []
    for _ in range(3):
        call(['/bin/sync'])
        time.sleep(2)
        info = plistlib.loads(call(['diskutil', 'info', '-plist', str(mount)]))
        samples.append(info['APFSContainerSize'] - info['APFSContainerFree'])
    row = dict(trial=trial, variant=variant, phase=phase, used_bytes=samples[-1], samples=samples)
    report['rows'].append(row)
    (root/'results.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(row), flush=True)

for trial in range(a.repeats):
    variants = [('baseline', a.before), ('native', a.after)]
    if trial % 2: variants.reverse()
    for variant, binary in variants:
        name = f'{variant}-{trial}'
        image = root/(name+'.sparseimage')
        mount = root/name
        mount.mkdir()
        call(['hdiutil', 'create', '-size', '2g', '-type', 'SPARSE', '-fs', 'Case-sensitive APFS', '-nospotlight', '-volname', name, str(image)])
        call(['hdiutil', 'attach', '-nobrowse', '-mountpoint', str(mount), str(image)])
        try:
            record(mount, trial, variant, 'empty')
            src = mount/'source'
            src.mkdir()
            for n in range(10000):
                f = src/f'd{n//100:05}'/f'f{n:07}.txt'
                f.parent.mkdir(exist_ok=True)
                f.write_bytes((f'file {n:07} stable line\n'.encode()*200)[:4096])
            with (src/'large.bin').open('wb') as f:
                for n in range(64):
                    f.write(hashlib.shake_256(str(n).encode()).digest(1024*1024))
            record(mount, trial, variant, 'source')
            env = dict(os.environ, LAYR_ROOT=str(mount/'store'), LAYR_AUTHOR_NAME='Benchmark', LAYR_AUTHOR_EMAIL='bench@example.invalid')
            for key in ('LAYR_SOCKET', 'LAYR_PROJECT', 'LAYR_LINE', 'LAYR_APFS_EXPERIMENT_CLONE_TREE'):
                env.pop(key, None)
            if variant == 'native': env['LAYR_APFS_EXPERIMENT_CLONE_TREE'] = '1'
            def layr(cwd, *args):
                return call([binary, *args], cwd=cwd, env=env)
            layr(mount, 'init', 'space', '--from', str(src), '-q')
            main = mount/'store/projects/space/lines/main'
            record(mount, trial, variant, 'init')
            for n in range(5): layr(main, 'line', 'new', '-q', f'b{n}')
            record(mount, trial, variant, 'five_branches')
            for n in range(5):
                work = main.parent/f'b{n}'
                for k in range(10):
                    f = work/'d00000'/f'f{k:07}.txt'
                    with f.open('r+b') as out: out.write(bytes([65+n])*4096)
                with (work/'large.bin').open('r+b') as out:
                    out.seek(n*1024*1024)
                    out.write(bytes([65+n])*4096)
                layr(work, 'add', '-A')
                layr(work, 'save', '-qm', 'space measurement')
            record(mount, trial, variant, 'five_commits')
            layr(main, 'fsck')
            assert (main/'large.bin').read_bytes() == (src/'large.bin').read_bytes()
            report.setdefault('verified', []).append(name)
            (root/'results.json').write_text(json.dumps(report, indent=2)+'\n')
        finally:
            call(['hdiutil', 'detach', str(mount)])
