#!/bin/bash
# Two local machines, one file store, Desk person keys and accepted pins.
. "$(dirname "$0")/lib.sh"
l init owner >/dev/null || exit 1
cd "$LAYR_ROOT/projects/owner/lines/main"
printf 'person data\n' > notes.txt
l add notes.txt || exit 1
l save -m 'First state' >/dev/null || exit 1
l remote add archive "file://$T/archive" --paper-key-out "$T/store-paper" >/dev/null || exit 1
l push archive -q || exit 1
export PERSON_TEST_ROOT="$T" PERSON_TEST_BINARY="$LAYR"
python3 - <<'PY'
import json,os,pathlib,subprocess,http.client,re,time,hashlib
root=pathlib.Path(os.environ['PERSON_TEST_ROOT'])
binary=os.environ['PERSON_TEST_BINARY']
base=os.environ.copy()
owner=dict(base,LAYR_PERSON_DIR=str(root/'owner-person'),LAYR_PERSON_LOGIN='owner@example.com')
carol=dict(base,LAYR_ROOT=str(root/'store/carol-root'),LAYR_PERSON_DIR=str(root/'carol-person'),LAYR_PERSON_LOGIN='carol@example.com')
stranger=dict(base,LAYR_ROOT=str(root/'store/stranger-root'),LAYR_PERSON_DIR=str(root/'carol-person'),LAYR_PERSON_LOGIN='stranger@example.com')
for env in [carol,stranger]:
 env.pop('LAYR_PROJECT',None);env.pop('LAYR_LINE',None)
checks=0
def check(value,label):
 global checks
 assert value,label
 checks+=1
 print('PASS',label)
def run(args,env=owner,success=True):
 p=subprocess.run([binary,*args],env=env,text=True,capture_output=True,timeout=30,cwd=root)
 if success: assert p.returncode==0,(args,p.stderr)
 return p
def refused(args,env,label,words=()):
 p=run(args,env,False)
 check(p.returncode!=0 and all(w in p.stderr for w in words),(label,p.stderr))
 return p
run(['person','keys','--paper-key-out',str(root/'owner-paper')])
run(['person','keys','--paper-key-out',str(root/'carol-paper')],carol)
owner['LAYR_PROJECT']='owner';owner['LAYR_PERSON_KEY_FILE']=str(root/'owner-paper')
carol['LAYR_PERSON_KEY_FILE']=str(root/'carol-paper')
stranger['LAYR_PERSON_KEY_FILE']=str(root/'carol-paper')
orec=json.loads(run(['person','show','--json']).stdout)
crec=json.loads(run(['person','show','--json'],carol).stdout)
check(orec['login']=='owner@example.com' and len((root/'owner-paper').read_text().split())==24,'person keys have 24 words and a public login')
check(all(p.stat().st_mode&0o777==0o600 for d in ['owner-person','carol-person'] for p in (root/d).iterdir()),'person files have mode 0600')
check(not any(b'AGE-SECRET-KEY-' in p.read_bytes() for d in ['owner-person','carol-person'] for p in (root/d).iterdir()),'person identity is never stored in plain form')
refused(['remote','grant','archive','--person','carol@example.com','--recipient',crec['recipient'],'--invite-out',str(root/'first.age')],owner,'the first grant must name the owner',['grant yourself first'])
check(not list((root/'archive/slots').glob('person-*')) and not (root/'first.age').exists(),'a refused first grant writes nothing')
missing=dict(owner);missing.pop('LAYR_PERSON_LOGIN')
refused(['remote','grant','archive','--person','owner@example.com','--recipient',orec['recipient']],missing,'local grants need an explicit login',['LAYR_PERSON_LOGIN'])
run(['remote','grant','archive','--person','owner@example.com','--recipient',orec['recipient'],'--invite-out',str(root/'owner.age')])
run(['person','accept','--invite-file',str(root/'owner.age')])
# Grant with the owner person slot, with no machine slot and no store paper key.
machine=(pathlib.Path(base['LAYR_ROOT'])/'machine/id').read_text().strip()
(root/f'archive/slots/machine-{machine}.age').unlink()
run(['remote','grant','archive','--person','carol@example.com','--recipient',crec['recipient'],'--invite-out',str(root/'carol.age')])
check((root/('archive/slots/person-carol@example.com-'+hashlib.sha256(crec['recipient'].encode()).hexdigest()[:16]+'.age')).exists(),'the owner person slot can grant Carol')
before={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'archive/slots').iterdir()}
repeat=run(['remote','grant','archive','--person','carol@example.com','--recipient',crec['recipient'],'--invite-out',str(root/'carol-retry.age')])
check('already granted' in repeat.stdout and before=={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'archive/slots').iterdir()},'a repeated grant leaves the existing slot unchanged')
check((root/'carol-retry.age').exists(),'a repeated grant issues another sealed invite')
refused(['clone','file://'+str(root/'archive'),'unaccepted','-q'],carol,'an invite needs explicit acceptance',['person pin'])
run(['person','accept','--invite-file',str(root/'carol.age')],carol)
check(not (root/'carol-person/addresses.json').exists(),'an invite without a URL keeps the old index format')
run(['person','accept','--invite-file',str(root/'carol.age'),'--remote','file://'+str(root/'archive')],carol)
addresses=json.loads((root/'carol-person/addresses.json').read_text())
check(len(addresses)==1 and next(iter(addresses.values()))['url']=='file://'+str(root/'archive'),'repeat acceptance records the validated store address')
check((root/'carol-person/addresses.json').stat().st_mode&0o777==0o600,'the person address file has mode 0600')
shown=json.loads(run(['person','show','--json'],carol).stdout)
check(len(shown['stores'])==1,'person show lists the accepted store')
run(['clone','file://'+str(root/'archive'),'copy','-q'],carol)
check((root/'store/carol-root/projects/copy/lines/main/notes.txt').read_text()=='person data\n','Carol clones with only her person key')
run(['--project','copy','fetch','origin','-q'],carol)
check(True,'Carol fetches with her person key')
# The same person identity under a login without a slot must not open a fresh machine.
refused(['clone','file://'+str(root/'archive'),'hidden','-q'],stranger,'a login without a slot cannot open',['no person slot'])
# A changed descriptor must fail before a slot opens and must name both recipients.
path=root/'archive/store.json';good=path.read_bytes();descriptor=json.loads(good)
bad=dict(descriptor,recipient=orec['recipient']);path.write_text(json.dumps(bad))
try:
 refused(['clone','file://'+str(root/'archive'),'wrong-pin','-q'],carol,'clone checks the person pin',[descriptor['recipient'],bad['recipient']])
 refused(['--project','copy','fetch','origin','-q'],carol,'fetch checks the person pin',[descriptor['recipient'],bad['recipient']])
 refused(['mount','file://'+str(root/'archive'),'--listen','127.0.0.1:0'],carol,'mount checks the person pin',[descriptor['recipient'],bad['recipient']])
finally: path.write_bytes(good)
# Remove the optional clone machine slot, so mount must use Carol's person slot.
cmachine=(root/'store/carol-root/machine/id').read_text().strip()
slot=root/f'archive/slots/machine-{cmachine}.age'
if slot.exists(): slot.unlink()
carol['XDG_CONFIG_HOME']=str(root/'mount-config');carol['XDG_CACHE_HOME']=str(root/'mount-cache')
log=open(root/'mount.log','w+')
process=subprocess.Popen([binary,'mount','file://'+str(root/'archive'),'--listen','127.0.0.1:0'],env=carol,stdout=log,stderr=log,cwd=root)
try:
 deadline=time.monotonic()+20
 while time.monotonic()<deadline:
  log.seek(0);text=log.read();match=re.search(r'http://127\.0\.0\.1:(\d+)/',text)
  if match: break
  assert process.poll() is None,text
  time.sleep(.05)
 else: raise AssertionError('mount did not start')
 connection=http.client.HTTPConnection('127.0.0.1',int(match[1]),timeout=10)
 connection.request('GET','/main/notes.txt');response=connection.getresponse();body=response.read();connection.close()
 check(response.status==200 and body==b'person data\n','mount reads the store through the person slot')
finally:
 process.terminate();process.wait(timeout=10);log.close()
# A damaged encrypted index fails closed in the actual person command.
index=root/'carol-person/index.age';data=index.read_bytes();index.write_bytes(data[:-1]+bytes([data[-1]^1]))
refused(['person','show','--json'],carol,'a changed person index is refused')
index.write_bytes(data)
print(f'person checks: {checks}')
PY
if [ "$?" -eq 0 ]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); fi
finish
