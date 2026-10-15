#!/usr/bin/env python3
"""Check APFS interoperability with a Linux layr binary through OrbStack."""
import argparse
import os
import pathlib
import shlex
import shutil
import subprocess
import tempfile

p = argparse.ArgumentParser()
p.add_argument('--mac-binary', required=True)
p.add_argument('--linux-binary', required=True)
p.add_argument('--volume', required=True)
p.add_argument('--vm', default='layr-dev')
a = p.parse_args()
mac = str(pathlib.Path(a.mac_binary).resolve())
orb = shutil.which('orb')
if not orb:
    raise SystemExit('OrbStack CLI is unavailable')
bridge = [orb, '-m', a.vm, '-u', 'root']

def run(args, **kw):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kw).strip()
    except subprocess.CalledProcessError as e:
        print(e.output, flush=True)
        raise

def linux(args):
    return run(bridge + args)

root = pathlib.Path(tempfile.mkdtemp(prefix='layr-cross-', dir=a.volume))
remote = linux(['mktemp', '-d', '/var/tmp/layr-cross.XXXXXX'])
env = dict(os.environ, LAYR_ROOT=str(root / 'store'))
env.pop('LAYR_SOCKET', None)
def m(*args):
    return run([mac, *args], env=env)
def b(*args):
    return linux(['env', 'LAYR_ROOT=' + remote + '/store', a.linux_binary, *args])
try:
    src = root / 'src'
    src.mkdir()
    (src / 'file').write_text('original\n')
    (src / 'empty').mkdir()
    (src / 'link').symlink_to('file')
    m('init', 'cross', '--from', str(src), '-q')
    work = root / 'store/projects/cross/lines/main'
    os.chdir(work)
    m('protect', 'main', '--remove')
    (work / 'file').write_text('on mac\n')
    m('commit', '-qam', 'mac commit')
    head = m('rev-parse', 'HEAD')
    peer = bridge + ['env', 'LAYR_ROOT=' + remote + '/store', a.linux_binary, 'serve-peer', 'cross']
    m('sync', '-q', '--', *peer)
    assert b('--project', 'cross', '--line', 'main', 'rev-parse', 'HEAD') == head
    b('--project', 'cross', 'adopt', 'main')
    line = remote + '/store/projects/cross/lines/main'
    assert linux(['cat', line + '/file']) == 'on mac'
    linux(['sh', '-c', 'printf "on linux\\n" > ' + shlex.quote(line + '/file')])
    b('-C', line, 'commit', '-qam', 'linux commit')
    linux_head = b('-C', line, 'rev-parse', 'HEAD')
    m('sync', '-q', '--', *peer)
    assert m('show', linux_head + ':file') == 'on linux'
    assert '0 problem(s)' in m('fsck')
    print('PASS APFS sends to Linux and receives a native incremental stream', flush=True)
    # Restore a macOS backup with the unchanged Linux binary.
    backup = root / 'backup'
    m('backup', '-q', str(backup))
    translated = '/mnt/mac' + str(backup)
    linux(['env', 'LAYR_ROOT=' + remote + '/restored', a.linux_binary, 'backup', 'restore', '-q', translated])
    restored = ['env', 'LAYR_ROOT=' + remote + '/restored', a.linux_binary, '--project', 'cross', '--line', 'main']
    assert linux(restored + ['show', head + ':file']) == 'on mac'
    assert linux(restored + ['show', linux_head + ':file']) == 'on linux'
    assert '0 problem(s)' in linux(restored + ['fsck'])
    print('PASS Linux restores the APFS backup with the same state identifiers', flush=True)
    native_backup = str(root / 'linux-backup')
    b('-C', line, 'backup', '-q', '/mnt/mac' + native_backup)
    restored_env = dict(env, LAYR_ROOT=str(root / 'from-linux'))
    run([mac, 'backup', 'restore', '-q', native_backup], env=restored_env)
    assert run([mac, '--project', 'cross', '--line', 'main', 'show', head + ':file'], env=restored_env) == 'on mac'
    assert run([mac, '--project', 'cross', '--line', 'main', 'show', linux_head + ':file'], env=restored_env) == 'on linux'
    assert '0 problem(s)' in run([mac, '--project', 'cross', '--line', 'main', 'fsck'], env=restored_env)
    print('PASS APFS restores the native Linux backup and verifies signatures', flush=True)
finally:
    os.chdir('/')
    subprocess.run(['chflags', '-R', 'nouchg', str(root)], check=False)
    subprocess.run(['chmod', '-R', 'u+rwX', str(root)], check=False)
    subprocess.run(['rm', '-rf', str(root)], check=True)
    # btrfs subvolumes need their readonly flag cleared before deletion.
    linux(['python3', '-c', '''import os,subprocess,sys
root=sys.argv[1]
for base,dirs,files in os.walk(root,topdown=False):
 for name in dirs:
  p=os.path.join(base,name)
  r=subprocess.run(['btrfs','subvolume','show',p],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  if r.returncode==0:
   subprocess.run(['btrfs','subvolume','delete',p],check=True,stdout=subprocess.DEVNULL)
import shutil
shutil.rmtree(root)
''', remote])
