#!/bin/bash
# Private overlays cross stores and peers, with separate read keys and service access.
. "$(dirname "$0")/lib.sh"
set -e
for u in layr-ov-reader layr-ov-other; do id "$u" >/dev/null 2>&1 || useradd -m "$u"; done
export OVERLAY_TEST_ROOT="$T" OVERLAY_TEST_BINARY="$LAYR"
python3 - <<'PY'
import hashlib,http.client,json,os,pathlib,re,subprocess,time
root=pathlib.Path(os.environ['OVERLAY_TEST_ROOT'])
binary=os.environ['OVERLAY_TEST_BINARY']
base=os.environ.copy()
base.pop('LAYR_SOCKET',None)
base.pop('LAYR_PROJECT',None)
base.pop('LAYR_LINE',None)
checks=0
A=dict(base,LAYR_PROJECT='project')
B=dict(base,LAYR_ROOT=str(root/'store/b-root'))
C=dict(base,LAYR_ROOT=str(root/'store/c-root'))
D=dict(base,LAYR_ROOT=str(root/'store/d-root'))
E=dict(base,LAYR_ROOT=str(root/'store/e-root'),LAYR_PERSON_DIR=str(root/'reader-person'),LAYR_PERSON_LOGIN='reader@example.com')
def run(args,env=A,success=True):
 p=subprocess.run([binary,*args],env=env,cwd=root,text=True,capture_output=True,timeout=90)
 if success: assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
 return p
def check(value,label):
 global checks
 assert value,label
 checks+=1
 print('PASS',label,flush=True)
def hashes():
 return {str(p.relative_to(root/'archive')):hashlib.sha256(p.read_bytes()).hexdigest()
  for p in (root/'archive').rglob('*') if p.is_file()}
def machine(env): return json.loads(run(['remote','self','--json'],env).stdout)
a=machine(A);b=machine(B);c=machine(C);d=machine(D)
run(['init','project','-q'])
line=pathlib.Path(A['LAYR_ROOT'])/'projects/project/lines/main'
(line/'public.txt').write_text('shared bytes\n')
run(['--line','main','add','public.txt'])
run(['--line','main','save','-m','Shared file','-q'])
run(['remote','add','archive','file://'+str(root/'archive'),'--paper-key-out',str(root/'paper')])
for who in [b,c,d]: run(['remote','trust','archive',who['id'],'--public-key',who['public_key']])
run(['person','keys','--paper-key-out',str(root/'person-paper')],E)
E['LAYR_PERSON_KEY_FILE']=str(root/'person-paper')
person=json.loads(run(['person','show','--json'],E).stdout)
run(['remote','grant','archive','--person','reader@example.com','--recipient',person['recipient'],'--invite-out',str(root/'reader-invite.age')],dict(A,LAYR_PERSON_LOGIN='reader@example.com'))
run(['person','accept','--invite-file',str(root/'reader-invite.age')],E)
run(['--line','main','overlay','create','secrets','--readers',f"layr-ov-reader,machine:{a['id']},machine:{b['id']},person:reader@example.com={person['recipient']}",'-q'])
(line/'.env').write_text('PRIVATE_TEST_VALUE=private-epoch-one\n')
run(['--line','main','overlay','move','secrets','.env','--purge','-q'])
run(['--line','main','save','-m','Private file','-q'])
report=json.loads(run(['push','archive','--json']).stdout)
check(not any('private overlay files' in x for x in report['omitted']),'push includes overlay files')
keyfiles=sorted((root/'archive/overlays').glob('*/keys/*.json'))
check(len(keyfiles)==1,'push publishes one signed layer key')
key=json.loads(keyfiles[0].read_text())
check(key['r']['op']=='overlay.key' and bool(key['s']),'the layer key is a signed overlay record')
k=key['r']['data'];epoch=root/f"archive/overlays/{k['overlay']}/epochs/{k['epoch']}"
check((epoch/f"slots/machine-{b['id']}.age").exists(),'the reader machine gets an epoch slot')
check(not (epoch/f"slots/machine-{c['id']}.age").exists(),'the non-reader has no layer slot')
check((epoch/'slots/paper.age').exists(),'the overlay owner gets a paper slot')
fp=hashlib.sha256(person['recipient'].encode()).hexdigest()[:16]
check((epoch/f'slots/person-reader@example.com-{fp}.age').exists(),'the person layer slot uses the v2 fingerprint name')
before=hashes();run(['push','archive','-q'])
check(hashes()==before,'a repeated push preserves every existing object')
run(['clone','file://'+str(root/'archive'),'project','-q'],B)
check((pathlib.Path(B['LAYR_ROOT'])/'projects/project/lines/main/.env').read_text().endswith('private-epoch-one\n'),'B clones the private overlay with its machine slot')
run(['clone','file://'+str(root/'archive'),'project','-q'],E)
check((pathlib.Path(E['LAYR_ROOT'])/'projects/project/lines/main/.env').read_text().endswith('private-epoch-one\n'),'a person reader clones the overlay with its person key')
report=json.loads(run(['clone','file://'+str(root/'archive'),'project','--json'],C).stdout)
check('overlay secrets: no key on this machine' in report['omitted'],'C receives the no-key report')
cline=pathlib.Path(C['LAYR_ROOT'])/'projects/project/lines/main'
check((cline/'public.txt').read_text()=='shared bytes\n' and not (cline/'.env').exists(),'C clones shared files only')
# C has the vault paper key for this check, but no overlay slot for its machine.
# Neither the vault identity nor C's machine identity opens any overlay object.
fixture=dict(A,LAYR_TEST_OVERLAY_DIR=str(root/'archive'),LAYR_TEST_OVERLAY_C_ROOT=C['LAYR_ROOT'],LAYR_TEST_OVERLAY_A_ROOT=A['LAYR_ROOT'])
subprocess.run(['cargo','test','--bin','layr','store_remote::overlays::tests::overlay_ciphertexts_reject_nonreader_keys','--','--ignored','--exact'],env=fixture,cwd=pathlib.Path(binary).parents[2],check=True,stdout=subprocess.DEVNULL)
check(True,'C cannot decrypt any encrypted overlay object')
# A live mount merges the overlay into the line view for B only.
def mount_read(env,path):
 env=dict(env,XDG_CONFIG_HOME=str(root/('mount-config-'+pathlib.Path(env['LAYR_ROOT']).name)),XDG_CACHE_HOME=str(root/('mount-cache-'+pathlib.Path(env['LAYR_ROOT']).name)))
 log=open(root/('mount-'+pathlib.Path(env['LAYR_ROOT']).name+'.log'),'w+')
 process=subprocess.Popen([binary,'mount','file://'+str(root/'archive'),'--listen','127.0.0.1:0'],env=env,cwd=root,stdout=log,stderr=log)
 try:
  deadline=time.monotonic()+20
  while time.monotonic()<deadline:
   log.seek(0);text=log.read();match=re.search(r'http://127\.0\.0\.1:(\d+)/',text)
   if match: break
   assert process.poll() is None,text
   time.sleep(.05)
  else: raise AssertionError('mount did not start')
  connection=http.client.HTTPConnection('127.0.0.1',int(match[1]),timeout=10)
  connection.request('GET',path);response=connection.getresponse();data=response.read();connection.close()
  return response.status,data
 finally:
  process.terminate();process.wait(timeout=10);log.close()
check(mount_read(B,'/main/.env')==(200,b'PRIVATE_TEST_VALUE=private-epoch-one\n'),'B reads the overlay through mount')
check(mount_read(C,'/main/.env')[0]==404,'mount hides the overlay from C')
check(mount_read(E,'/main/.env')==(200,b'PRIVATE_TEST_VALUE=private-epoch-one\n'),'a person reader mounts the overlay with its person slot')
# A peer outside the default machine reader set receives shared files only.
run(['remote','add','nonreader','ssh://test/project'])
blocked=json.loads(run(['sync','nonreader','--json','--command','--','env','LAYR_ROOT='+C['LAYR_ROOT'],binary,'serve-peer','project']).stdout)
check('overlay secrets: peer is not allowed to hold it' in blocked['omitted'],'default peers exclude C')
check(not (pathlib.Path(C['LAYR_ROOT'])/'projects/project/meta/overlays/secrets').exists(),'an excluded peer receives no overlay project')
# Peer B receives overlay states. C is explicitly allowed to hold them, but its service enforces readers.
for who,env in [(b,B),(c,C)]:
 name='peer-'+who['id'][-8:]
 run(['remote','add',name,'ssh://test/project'])
 if who==c: run(['config','overlay.secrets.peers',json.dumps([b['id'],c['id']])])
 run(['sync',name,'--command','--','env','LAYR_ROOT='+env['LAYR_ROOT'],binary,'serve-peer','project'])
 check((pathlib.Path(env['LAYR_ROOT'])/'projects/project/meta/overlays/secrets/meta/db/layr.sqlite').exists(),'peer '+who['id'][-8:]+' receives the overlay project')
# A service on C must refuse the non-reader and permit the local user who reads the overlay.
import shutil
shutil.copy2(binary,root/'layr');os.chmod(root/'layr',0o755)
csocket=root/'c.sock';daemonlog=open(root/'c-daemon.log','w')
daemon=subprocess.Popen([binary,'daemon','--socket',str(csocket),'--tick','3600'],env=C,stdout=daemonlog,stderr=daemonlog,cwd=root)
try:
 for _ in range(100):
  if csocket.exists(): break
  assert daemon.poll() is None
  time.sleep(.05)
 os.chmod(root,0o755);os.chmod(pathlib.Path(C['LAYR_ROOT']),0o755)
 for user,allowed in [('layr-ov-reader',True),('layr-ov-other',False)]:
  p=subprocess.run(['runuser','-u',user,'--','env','LAYR_SOCKET='+str(csocket),'LAYR_PROJECT=project',str(root/'layr'),'overlay','show','secrets','user-0:.env'],capture_output=True,text=True,cwd='/')
  check((p.returncode==0)==allowed,'C service enforces overlay readers for '+user)
finally:
 daemon.terminate();daemon.wait(timeout=10);daemonlog.close()
# A machine that holds peer plaintext but has no layer slot can push to the signed recipient.
run(['remote','add','writer','ssh://test/project'])
run(['config','overlay.secrets.peers',json.dumps([b['id'],c['id'],d['id']])])
run(['sync','writer','--command','--','env','LAYR_ROOT='+D['LAYR_ROOT'],binary,'serve-peer','project'])
run(['--project','project','adopt','main'],D)
dline=pathlib.Path(D['LAYR_ROOT'])/'projects/project/lines/main'
(dline/'.env').write_text('PRIVATE_TEST_VALUE=write-only-machine\n')
run(['--project','project','--line','main','save','-am','Write-only overlay update','-q'],D)
run(['--project','project','remote','add','archive','file://'+str(root/'archive')],D)
run(['--project','project','push','archive','-q'],D)
check(not (epoch/f"slots/machine-{d['id']}.age").exists(),'a write-only push adds no slot for its own machine')
check(len(list((epoch/'states').glob('*/*.tree.age')))>=3,'a write-only machine publishes encrypted overlay states')
check(mount_read(B,'/main/.env')==(200,b'PRIVATE_TEST_VALUE=write-only-machine\n'),'mount uses the newest overlay-only line update')
# Removal produces a new key, fresh objects and slots for the remaining readers only.
old=hashes()
run(['--line','main','overlay','readers','secrets',f"machine:{a['id']}"])
(line/'.env').write_text('PRIVATE_TEST_VALUE=private-epoch-two\n')
run(['--line','main','save','-am','New private data','-q'])
run(['push','archive','-q'])
keyfiles=sorted((root/'archive/overlays').glob('*/keys/*.json'))
check(len(keyfiles)==2,'reader removal starts a new epoch')
latest=max((json.loads(p.read_text())['r'] for p in keyfiles),key=lambda r:(r['time'],r['machine'],r['seq']))['data']
newepoch=root/f"archive/overlays/{latest['overlay']}/epochs/{latest['epoch']}"
check(latest['epoch']!=k['epoch'] and latest['recipient']!=k['recipient'],'the new epoch has a new layer recipient')
check(not (newepoch/f"slots/machine-{b['id']}.age").exists(),'the removed reader has no new epoch slot')
check(all(hashes().get(path)==digest for path,digest in old.items()),'rotation preserves all earlier objects')
report=json.loads(run(['--project','project','fetch','origin','--json'],B).stdout)
check('overlay secrets: no key on this machine' in report['omitted'],'B cannot fetch the new epoch')
check(run(['--project','project','overlay','show','secrets','user-0:.env'],B).stdout.endswith('private-epoch-one\n'),'B retains only data from its earlier epoch')
(root/'check-count').write_text(str(checks))
print(f'overlay checks: {checks}')
PY
PASS=$(cat "$T/check-count")
finish
