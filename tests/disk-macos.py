#!/usr/bin/env python3
"""Check a disposable macOS disk fixture with main/hello.txt = 'native WebDAV' plus a newline.

Usage: tests/disk-macos.py <store-url> <paper-file> [--read-mounted]
The default checks native mount metadata and reads file bytes through HTTP.
Use --read-mounted to also require file reads through macOS WebDAVFS.
The test refuses an existing disk agent or ~/Layr folder.
"""
import argparse,hashlib,http.client,os,pathlib,plistlib,re,signal,socket,subprocess,tempfile,time
parser=argparse.ArgumentParser()
parser.add_argument('url');parser.add_argument('paper');parser.add_argument('--read-mounted',action='store_true')
options=parser.parse_args()
assert os.uname().sysname=='Darwin', 'this test needs macOS'
root=pathlib.Path(tempfile.mkdtemp(prefix='layr-disk-native-'))
binary=str(pathlib.Path(os.environ.get('LAYR','target/debug/layr')).resolve());url=options.url
paper=pathlib.Path(options.paper).resolve()
home=pathlib.Path.home(); assert not (home/'Layr').exists(), 'HOME/Layr already exists'
env=os.environ.copy(); env.update(XDG_CONFIG_HOME=str(root/'config'),LAYR_ROOT=str(root/'no-project-store'),LAYR_SOCKET=str(root/'no-service'))
label='ws.layr.mount'; domain='gui/'+str(os.getuid())
existing=subprocess.run(['/bin/launchctl','print',domain+'/'+label],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
assert existing.returncode!=0,'an existing disk agent prevents the isolated install test'
log=open(root/'native.log','w+'); process=None; agent=False
try:
 args=[binary,'mount',url,'--paper-key-file',str(paper),'--cache',str(root/'cache'),'--listen','127.0.0.1:0','--at',str(root/'manual')]
 process=subprocess.Popen(args,env=env,stdout=log,stderr=log,start_new_session=True)
 deadline=time.monotonic()+45
 while time.monotonic()<deadline:
  if (root/'manual/main/hello.txt').exists(): break
  if process.poll() is not None: raise AssertionError('the native disk stopped')
  time.sleep(.2)
 else: raise AssertionError('the macOS WebDAV client did not mount the disk')
 assert (root/'manual/main/hello.txt').stat().st_size==14
 if options.read_mounted: assert (root/'manual/main/hello.txt').read_bytes()==b'native WebDAV\n'
 try:
  (root/'manual/refused.txt').write_text('refused')
 except OSError: pass
 else: raise AssertionError('the native disk accepted a write')
 subprocess.run(['/sbin/umount',str(root/'manual')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
 os.killpg(process.pid,signal.SIGTERM); process.wait(timeout=10); process=None
 print('PASS native WebDAV mount, file metadata and write refusal')
 # The same store opens through the login Keychain, with no paper input argument.
 sock=socket.socket(); sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]; sock.close()
 args=[binary,'mount','--install',url,'--listen','127.0.0.1:'+str(port),'--cache',str(root/'cache')]
 result=subprocess.run(args,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=30)
 assert result.returncode==0,'launchd install failed'
 agent=True
 plist=home/'Library/LaunchAgents/ws.layr.mount.plist'
 subprocess.run(['/usr/bin/plutil','-lint',str(plist)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
 data=plistlib.loads(plist.read_bytes())
 assert data['EnvironmentVariables']['HOME']==str(home)
 assert not any('paper-key' in arg for arg in data['ProgramArguments'])
 assert not any(key.startswith('AWS_') for key in data['EnvironmentVariables'])
 deadline=time.monotonic()+45
 while time.monotonic()<deadline:
  if (home/'Layr/main/hello.txt').exists(): break
  time.sleep(.2)
 else: raise AssertionError('the login agent did not mount at HOME/Layr')
 assert (home/'Layr/main/hello.txt').stat().st_size==14
 if options.read_mounted: assert (home/'Layr/main/hello.txt').read_bytes()==b'native WebDAV\n'
 c=http.client.HTTPConnection('127.0.0.1',port,timeout=10);c.request('GET','/main/hello.txt');r=c.getresponse();assert r.status==200 and r.read()==b'native WebDAV\n';c.close()
 assert not (root/'no-project-store').exists()
 assert paper.read_bytes().strip() not in plist.read_bytes()
 print('PASS Keychain restart, launchd install and HOME/Layr mount')
 removed=subprocess.run([binary,'mount','--remove'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=30)
 assert removed.returncode==0 and not plist.exists(),'launchd removal failed'
 agent=False
 check=subprocess.run(['/bin/launchctl','print',domain+'/'+label],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 assert check.returncode!=0,'the removed agent remains loaded'
 print('PASS launchd removal')
finally:
 if agent:
  subprocess.run([binary,'mount','--remove'],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
 if process is not None:
  subprocess.run(['/sbin/umount',str(root/'manual')],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  os.killpg(process.pid,signal.SIGTERM); process.wait(timeout=10)
 log.close()
 for at in [root/'manual', home/'Layr']:
  try: at.rmdir()
  except OSError: pass
 import shutil
 shutil.rmtree(root)
 account=hashlib.sha256(url.encode()).hexdigest()
 subprocess.run(['/usr/bin/security','delete-generic-password','-s','ws.layr.mount','-a',account],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
