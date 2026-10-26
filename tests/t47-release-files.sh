#!/bin/bash
# Built files bind to exact release refs, stream through stores, and verify before saving.
. "$(dirname "$0")/lib.sh"
set -e
chmod 755 "$T" "$LAYR_ROOT"
id layr-release-writer >/dev/null 2>&1 || useradd -m layr-release-writer
export LAYR_SOCKET=$T/layr.sock
cp "$LAYR" "$T/layr"; chmod 755 "$T/layr"
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 60 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
sample "$T/source"
l init releases --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/releases/lines/main"
l remote add origin "${REMOTE_URL:-file://$T/archive}" --paper-key-out "$T/store-paper" >/dev/null
new_fs "$T/replica" 2G
export RELEASE_TEST_ROOT="$T" RELEASE_TEST_BINARY="$LAYR"
python3 - <<'PY'
import hashlib,json,os,pathlib,subprocess
root=pathlib.Path(os.environ['RELEASE_TEST_ROOT']);binary=os.environ['RELEASE_TEST_BINARY'];base=os.environ.copy();cwd=pathlib.Path.cwd();checks=0

def run(args,env=base,ok=True):
 p=subprocess.run([binary,*args],cwd=cwd,env=env,text=True,capture_output=True,timeout=180)
 if ok:assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
 return p

def data(args,env=base):return json.loads(run(args,env).stdout)
def check(value,label):
 global checks
 assert value,label
 checks+=1;print('PASS',label,flush=True)
def refuse(args,label,words=()):
 p=run(args,ok=False);check(p.returncode!=0 and all(w in p.stderr for w in words),label)
 return p

name='release/1.2.0';first=run(['rev-parse','head']).stdout.strip()
run(['ref','set',name,first])
first=next(r['target'] for r in data(['ref','list','--json']) if r['name']==name)
payload=root/'layr-linux-arm64'
with payload.open('wb') as f:
 for _ in range(160):f.write(bytes(range(256))*256)
expected=hashlib.sha256(payload.read_bytes()).hexdigest()
files=data(['release','attach',name,str(payload),'--json'])
check(len(files)==1 and files[0]['size']==10*1024*1024 and files[0]['sha256']==expected,'attach publishes a signed 10 MiB file with the plaintext SHA-256')
check(files==data(['release','files',name,'--json']),'list returns the authenticated exact release list')
check(files[0]['object'].startswith('files/') and files[0]['object'].endswith('.age') and expected not in files[0]['object'],'object key is an opaque UUID rather than a content digest')
records=[json.loads(line)['r'] for line in run(['op','export']).stdout.splitlines()]
record=next(r for r in reversed(records) if r['op']=='release.files')
check(record['data']=={'reference':name,'state':first,'files':files},'signed record binds the exact ref, state and complete file metadata')
run(['release','get',name,'-o',str(root/'downloads')])
check(hashlib.sha256((root/'downloads'/payload.name).read_bytes()).hexdigest()==expected,'get round trip checks and saves the plaintext')
refuse(['release','get',name,'-o',str(root/'downloads')],'get preserves an existing output file')
check(hashlib.sha256((root/'downloads'/payload.name).read_bytes()).hexdigest()==expected,'failed replacement preserves existing bytes')
refuse(['release','get',name,'missing'],'get refuses an unknown filename')
refuse(['release','attach','env/prod',str(payload)],'attach refuses refs outside release/')
refuse(['release','attach',name,str(payload),'--name','../escape'],'attach refuses unsafe output names')
refuse(['release','attach',name,str(payload),str(payload)],'attach refuses duplicate filenames')
refuse(['release','attach',name,str(payload),str(payload),'--name','renamed'],'--name applies to exactly one file')
refuse(['release','attach',name,*([str(payload)]*65)],'attach enforces the 64-file limit')
large=root/'too-large'
with large.open('wb') as f:f.truncate(512*1024*1024+1)
refuse(['release','attach',name,str(large)],'attach enforces 512 MiB before encryption',['512 MiB'])
large.unlink()
# A writer cannot attach files to a release restricted to maintainers.
run(['access','grant','layr-release-writer','writer'])
run(['protect','release/*','--update','maintainer','--no-expect'])
p=subprocess.run(['runuser','-u','layr-release-writer','--','env','LAYR_ROOT='+base['LAYR_ROOT'],'LAYR_SOCKET='+base['LAYR_SOCKET'],str(root/'layr'),'--project','releases','release','attach',name,str(payload)],cwd='/tmp',text=True,capture_output=True,timeout=60)
assert p.returncode!=0 and 'update role maintainer' in p.stderr, (p.returncode,p.stdout,p.stderr)
check(True,'attach refuses a writer outside the release protection update role')
run(['protect','release/*','--update','writer','--no-expect'])
# A changed or truncated file must never become a visible output file.
archive=root/'archive'
if archive.exists():
 object=archive/files[0]['object'];original=object.read_bytes()
 check(original.startswith(b'age-encryption.org/v1') and b'AGE-SECRET-KEY-' not in original,'stored object is age ciphertext')
 for label,damaged in [('changed',original[:-1]+bytes([original[-1]^1])),('truncated',original[:len(original)//2])]:
  object.write_bytes(damaged)
  refuse(['release','get',name,'-o',str(root/label)],label+' ciphertext is refused')
  check(not (root/label/payload.name).exists() and not list((root/label).glob('.layr-release-*')),label+' ciphertext leaves no output or temporary file')
 object.write_bytes(original)
# A later signed list replaces the list, without replacing the old object.
second=data(['release','attach',name,str(payload),'--name','renamed-arm64','--json'])
check(second[0]['object']!=files[0]['object'] and second[0]['name']=='renamed-arm64','repeated content gets a fresh opaque object key')
check(data(['release','files',name,'--json'])==second,'later record replaces the exact-state file list')
run(['gc','--remote','origin','--json'])
run(['release','get',name,'renamed-arm64','-o',str(root/'after-gc')])
check(hashlib.sha256((root/'after-gc/renamed-arm64').read_bytes()).hexdigest()==expected,'GC keeps attached release files')
# A second authorized machine can move the remote ref while this checkout is stale.
peer=dict(base,LAYR_ROOT=str(root/'replica'));peer.pop('LAYR_SOCKET',None)
who=data(['remote','self','--json'],peer)
run(['remote','trust','origin',who['id'],'--public-key',who['public_key']])
url=os.environ.get('REMOTE_URL','file://'+str(archive))
run(['clone',url,'copy','-q'],peer)
peer['LAYR_PROJECT']='copy';peer['LAYR_LINE']='main'
peerline=root/'replica/projects/copy/lines/main'
(peerline/'remote.txt').write_text('moved on another machine\n')
run(['add','remote.txt'],peer);run(['save','-qm','remote release state'],peer)
remote_state=run(['rev-parse','head'],peer).stdout.strip()
run(['ref','set',name,remote_state],peer);run(['push','origin','-q'],peer)
refuse(['release','attach',name,str(payload)],'attach refuses a ref moved by another writer on the remote',['changed on the remote'])
run(['fetch','origin','-q'])
# Move the local ref and reject a stale expected state.
(cwd/'notes.txt').write_text('new release state\n');run(['add','notes.txt']);run(['save','-qm','new release state'])
next=run(['rev-parse','head']).stdout.strip();run(['ref','set',name,next])
refuse(['release','attach',name,str(payload),'--expect',first],'attach refuses a moved expected ref',['changed'])
# Push the new ref; stale lists must not appear for its new state.
run(['push','origin','-q'])
check(data(['release','files',name,'--json'])==[],'a moved ref exposes no attachment list for its previous state')
run(['release','attach',name,str(payload),'--expect',next])
# Public readers use the published identity and have no writer access.
run(['remote','mode','origin','public'])
run(['release','get',name,'--public','-o',str(root/'public')])
check(hashlib.sha256((root/'public'/payload.name).read_bytes()).hexdigest()==expected,'public reader downloads with the public project identity')
refuse(['release','attach',name,str(payload),'--public'],'public access cannot attach files')
# An optional disposable export feeds the real-gateway browser tests.
if os.environ.get('LAYR_RELEASE_EXPORT') and archive.exists():
 import shutil
 output=pathlib.Path(os.environ['LAYR_RELEASE_EXPORT']);output.mkdir(parents=True)
 huge=root/'large-file'
 with huge.open('wb') as f:f.truncate(256*1024*1024+1)
 bad=root/'sha-mismatch';bad.write_bytes(b'wrong-object-test'*(65536//17))
 final=data(['release','attach',name,str(payload),str(bad),str(huge),'--json'])
 shutil.copytree(archive,output/'store')
 (output/'fixture.json').write_text(json.dumps({'reference':name,'files':final,'sha256':expected,'project':record['project']}))
 check(True,'exports real signed release files for the gateway browser test')
# Tightening to a private epoch changes the vault used for new file objects.
run(['ref','set','release/previous',next])
old=data(['release','attach','release/previous',str(payload),'--json'])
run(['remote','mode','origin','private'])
current=data(['release','attach',name,str(payload),'--json'])
if archive.exists():
 matches=[p for p in (archive/'project/epochs').rglob('*.age') if p.name==pathlib.Path(current[0]['object']).name]
 check(len(matches)==1 and not (archive/current[0]['object']).exists(),'new files are written inside the current project epoch')
run(['release','get',name,'-o',str(root/'epoch-current')])
check(hashlib.sha256((root/'epoch-current'/payload.name).read_bytes()).hexdigest()==expected,'current-epoch file decrypts and verifies')
run(['release','get','release/previous','-o',str(root/'epoch-old')])
check(hashlib.sha256((root/'epoch-old'/payload.name).read_bytes()).hexdigest()==expected,'retained release file decrypts with its historical epoch identity')
run(['remote','mode','origin','public'])
run(['release','get',name,'--public','-o',str(root/'epoch-public')])
check(hashlib.sha256((root/'epoch-public'/payload.name).read_bytes()).hexdigest()==expected,'public opener downloads current-epoch files after rotation')
run(['ref','delete',name]);run(['ref','delete','release/previous']);run(['push','origin','-q'])
removed=data(['gc','--remote','origin','--json'])
check(any('files/' in str(row) for row in removed),'deleting and pushing the release ref lets GC free its objects')
if archive.exists():
 import uuid
 leftovers={str(p.relative_to(archive)) for p in archive.rglob('*.age') if p.parent.name=='files'}
 published={f['object'] for line in run(['op','export']).stdout.splitlines() if json.loads(line)['r']['op']=='release.files' for f in json.loads(line)['r']['data']['files']}
 check(leftovers.isdisjoint(published),'deleted release frees every published file across epochs')
 # A failed attach may leave a recent unreferenced upload. Its grace period is
 # independent of ref deletion and prevents collection during an active upload.
 orphan=archive/'files'/(str(uuid.uuid4())+'.age');orphan.parent.mkdir(exist_ok=True);orphan.write_bytes(original)
 run(['gc','--remote','origin','--json'])
 check(orphan.exists(),'GC retains a recent orphan for the upload grace period')
 for p in archive.rglob('*.age'):
  if p.parent.name=='files':os.utime(p,(1,1))
 run(['gc','--remote','origin','--json'])
 check(not [p for p in archive.rglob('*.age') if p.parent.name=='files'],'GC frees old orphan uploads after their grace period')
check('signatures ok' in run(['fsck']).stdout,'release history signatures verify')
print(f't47-release-files: {checks} passed, 0 failed',flush=True)
PY
