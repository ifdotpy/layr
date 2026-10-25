#!/bin/bash
# Project epochs, access modes, audience closure and overlay privacy through the CLI.
. "$(dirname "$0")/lib.sh"
set -e
for machine in owner removed reader fresh anonymous person upgrade; do new_fs "$T/$machine" 2G; done
export ROT_TEST_ROOT="$T" ROT_TEST_BINARY="$LAYR"
export ROT_TEST_PROBE="$(dirname "$LAYR")/examples/epoch_probe"
python3 - <<'PY'
import hashlib,http.client,json,os,pathlib,re,sqlite3,subprocess,time,uuid
root=pathlib.Path(os.environ['ROT_TEST_ROOT']);binary=os.environ['ROT_TEST_BINARY'];probe=os.environ['ROT_TEST_PROBE'];checks=0
base=os.environ.copy()
for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_PERSON_KEY_FILE','LAYR_PAPER_KEY_FILE','LAYR_GATEWAY_TOKEN_FILE']:base.pop(key,None)
envs={n:dict(base,LAYR_ROOT=str(root/n/'store')) for n in ['owner','removed','reader','fresh','anonymous','person','upgrade']}
A=envs['owner'];B=envs['removed'];C=envs['reader'];D=envs['fresh'];P=envs['anonymous']
def run(args,env=A,ok=True,input=None):
 p=subprocess.run([binary,*args],cwd=root,env=env,text=True,capture_output=True,input=input,timeout=120)
 if ok:assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
 return p
def check(value,label):
 global checks
 assert value,label
 checks+=1;print('PASS',label,flush=True)
def refuse(args,label,env=A,words=()):
 p=run(args,env,False)
 check(p.returncode!=0 and all(w in p.stderr for w in words),label)
 return p
def hashes():return {str(p.relative_to(root/'archive')):hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'archive').rglob('*') if p.is_file()}
def latest():
 rows=[json.loads(p.read_text())['r'] for p in (root/'archive/keys').glob('*.json')]
 return sorted(rows,key=lambda r:(r['time'],r['machine'],r['seq']))[-1]
def records(path):
 db=sqlite3.connect((path/'meta/db/layr.sqlite').as_uri()+'?mode=ro',uri=True)
 try:return [json.loads(r)['r'] for r, in db.execute('select raw from records')]
 finally:db.close()
def save(name,content):
 (line/name).write_text(content);run(['--line','main','add',name]);run(['--line','main','save','-qm',content.strip()]);return run(['--line','main','rev-parse','head']).stdout.strip()
a=json.loads(run(['remote','self','--json']).stdout);b=json.loads(run(['remote','self','--json'],B).stdout);c=json.loads(run(['remote','self','--json'],C).stdout)
run(['init','project','-q']);A['LAYR_PROJECT']='project';line=root/'owner/store/projects/project/lines/main';project=line.parent.parent
first=save('shared.txt','epoch zero\n')
run(['remote','add','archive','file://'+str(root/'archive'),'--paper-key-out',str(root/'paper')]);run(['push','archive','-q'])
for machine in [b,c]:run(['remote','trust','archive',machine['id'],'--public-key',machine['public_key']])
for env in [B,C]:run(['clone','file://'+str(root/'archive'),'project','-q'],env);env['LAYR_PROJECT']='project'
run(['push','archive','-q'])
E=dict(envs['person'],LAYR_PERSON_DIR=str(root/'person-identity'),LAYR_PERSON_LOGIN='removed@example.com')
run(['person','keys','--paper-key-out',str(root/'person-paper')],E)
E['LAYR_PERSON_KEY_FILE']=str(root/'person-paper')
person=json.loads(run(['person','show','--json'],E).stdout)
run(['remote','grant','archive','--person','removed@example.com','--recipient',person['recipient'],'--invite-out',str(root/'person-invite.age')],dict(A,LAYR_PERSON_LOGIN='removed@example.com'))
run(['person','accept','--invite-file',str(root/'person-invite.age')],E)
run(['push','archive','-q'])
person_slot=next((root/'archive/slots').glob('person-removed@example.com-*.age'))
old_object=next((root/'archive/records').rglob('*.age'))
check(subprocess.run([probe,'person',str(root/'person-paper'),str(root/'person-identity/paper.age'),str(person_slot),str(old_object)],capture_output=True).returncode==0,'person opener reads epoch-zero objects')
root_descriptor=(root/'archive/store.json').read_bytes();old_hashes=hashes()
refuse(['remote','mode','archive','invalid'],'invalid project mode is refused')
check(hashes()==old_hashes,'invalid project mode changes no objects')
refuse(['remote','mode','archive','service'],'service mode requires a configured recipient',words=['service-recipient'])
check(hashes()==old_hashes,'missing service configuration changes no objects')
# New overlays are private independently of parent opening.
run(['--line','main','overlay','create','visible','--readers','machine:'+a['id'],'-q'])
run(['--line','main','overlay','create','hidden-rotation-731','--hidden','--readers','machine:'+a['id'],'-q'])
(line/'visible-layer.txt').write_text('explicit public overlay bytes\n')
run(['--line','main','overlay','move','visible','visible-layer.txt','-q']);run(['--line','main','save','-qm','Visible overlay state'])
(line/'hidden-layer.txt').write_text('hidden layer bytes\n')
run(['--line','main','overlay','move','hidden-rotation-731','hidden-layer.txt','-q']);run(['--line','main','save','-qm','Hidden layer state'])
refuse(['overlay','mode','visible','public'],'private project refuses more-open overlay',words=['project mode private'])
check(json.loads(run(['overlay','show','visible','--json']).stdout)['access_mode']=='private','overlay defaults to private')
run(['push','archive','-q']);run(['remote','rotate','archive','--remove','machine:'+b['id'],'--remove','person:removed@example.com','--reason','reader removed'])
epoch=latest();prefix=root/'archive/project/epochs'/epoch['data']['epoch']
check(epoch['data']['parent']=='root' and epoch['op']=='project.key','rotation publishes signed child of epoch zero')
check(set(epoch['data'])=={'project','parent','parent_recipient','epoch','recipient','mode','readers_hash'},'discovery exposes only approved fields and reader hash')
check((root/'archive/store.json').read_bytes()==root_descriptor,'root trust anchor stays byte-identical')
check((prefix/'history/root.age').is_file(),'old identity is sealed into the current epoch')
check(not (prefix/f"slots/machine-{b['id']}.age").exists(),'removed machine has no current slot')
check(not list((prefix/'slots').glob('person-removed@example.com-*.age')),'removed person has no current slot')
new_object=next((prefix/'records').rglob('*.age'))
check(subprocess.run([probe,'person',str(root/'person-paper'),str(root/'person-identity/paper.age'),str(person_slot),str(old_object)],capture_output=True).returncode==0,'removed person still decrypts earlier epoch')
check(subprocess.run([probe,'person',str(root/'person-paper'),str(root/'person-identity/paper.age'),str(person_slot),str(new_object)],capture_output=True).returncode!=0,'removed person cannot decrypt current epoch with old vault identity')
refuse(['clone','file://'+str(root/'archive'),'denied','-q'],'removed person cannot clone current epoch',E,words=['current project epoch'])
check((prefix/f"slots/machine-{c['id']}.age").is_file(),'remaining machine receives current slot')
second=save('after.txt','after removal\n');run(['push','archive','-q'])
check('epoch zero' in run(['--line','main','show',first+':shared.txt'],B).stdout,'removed reader retains earlier data')
refuse(['fetch','origin','-q'],'removed reader cannot fetch current epoch',B,words=['current project epoch'])
before=hashes();refuse(['push','origin','-q'],'stale removed writer cannot publish to epoch zero',B,words=['current project epoch'])
check(before==hashes(),'stale removed writer changes no remote objects')
run(['fetch','origin','-q'],C)
check('after removal' in run(['--line','main','show',second+':after.txt'],C).stdout,'remaining reader fetches the new epoch')
# Paper slot starts a new machine and opens all old objects without rekeying them.
D['LAYR_PAPER_KEY_FILE']=str(root/'paper');run(['clone','file://'+str(root/'archive'),'project','-q'],D);D['LAYR_PROJECT']='project'
d=json.loads(run(['remote','self','--json'],D).stdout)
check(not (prefix/('records/'+d['id'])).exists() and (prefix/f"slots/machine-{d['id']}.key.age").is_file(),'fresh paper clone stores encrypted key metadata without publishing shared records')
check('epoch zero' in run(['--line','main','show',first+':shared.txt'],D).stdout,'fresh reader reads sealed earlier history')
check('after removal' in run(['--line','main','show',second+':after.txt'],D).stdout,'fresh reader reads current history')
check(all(hashes().get(k)==v for k,v in old_hashes.items()),'rotation never rewrites old objects')
run(['remote','rotate','archive','--reason','second rotation']);new=latest();newprefix=root/'archive/project/epochs'/new['data']['epoch']
check((newprefix/'history/root.age').is_file() and (newprefix/('history/'+epoch['data']['epoch']+'.age')).is_file(),'second rotation seals every earlier identity')
run(['fetch','origin','-q'],C);check(True,'remaining reader follows two rotations')
D.pop('LAYR_PAPER_KEY_FILE');run(['fetch','origin','-q'],D)
check('after removal' in run(['--line','main','show',second+':after.txt'],D).stdout,'never-pushed paper clone keeps machine read access after owner rotation')
check(not any(r['op']=='machine.trust' and r['data'].get('machine')==d['id'] for r in records(project)),'clone enrollment never grants writer trust')
# Discovery tampering and a sealed-history substitution fail through the real fetch caller.
keypath=root/'archive/keys'/(new['data']['epoch']+'-'+new['id']+'.json');original=keypath.read_bytes()
for field,value in [('mode','public'),('readers_hash','0'*64)]:
 changed=json.loads(original);changed['r']['data'][field]=value;keypath.write_text(json.dumps(changed))
 try:refuse(['fetch','origin','-q'],'fetch refuses tampered signed epoch '+field,C)
 finally:keypath.write_bytes(original)
changed=json.loads(original);changed['r']['data']['readers']={'person:leaked@example.com':'recipient'};keypath.write_text(json.dumps(changed))
try:refuse(['fetch','origin','-q'],'discovery refuses a plaintext reader map',C)
finally:keypath.write_bytes(original)
history=newprefix/'history/root.age';sealed=history.read_bytes();history.write_bytes((root/'archive/slots/paper.age').read_bytes())
try:refuse(['fetch','origin','-q'],'fetch refuses substituted historical identity',C)
finally:history.write_bytes(sealed)
# A new invite pins epoch zero but opens a slot granted only in the current epoch.
K=dict(A,LAYR_PERSON_DIR=str(root/'keeper-person'),LAYR_PERSON_LOGIN='keeper@example.com')
run(['person','keys','--paper-key-out',str(root/'keeper-paper'),'--store','file://'+str(root/'archive')],K)
K['LAYR_PERSON_KEY_FILE']=str(root/'keeper-paper')
keeper=json.loads(run(['person','show','--json'],K).stdout)
run(['remote','grant','archive','--person','keeper@example.com','--recipient',keeper['recipient'],'--invite-out',str(root/'keeper-invite.age')],dict(A,LAYR_PERSON_LOGIN='keeper@example.com'))
run(['person','accept','--invite-file',str(root/'keeper-invite.age'),'--remote','file://'+str(root/'archive')],K)
check(True,'new person accepts a current-epoch-only grant against the root pin')
run(['access','person-key','keeper@example.com',keeper['signing_key']]);run(['access','grant','person:keeper@example.com','writer'])
change=next(c['change_id'] for c in json.loads(run(['change','list','--json']).stdout) if c['message']=='epoch zero')
request=json.loads(run(['request','comment',change,'epoch request text','--person','--remote','archive','--json'],K).stdout)
request_path=newprefix/'requests'/(request['id']+'.json');sealed=json.loads(request_path.read_text())
check(sealed['v']==2 and sealed['epoch']==new['data']['epoch'] and 'epoch request text' not in request_path.read_text(),'request uses seal_for_epoch and current namespace with encrypted content')
check(not (root/'archive/requests'/(request['id']+'.json')).exists(),'current request never leaks into the root namespace')
# The service audience uses the same sealed reader-slot mechanism.
service=subprocess.check_output([probe,'new',str(root/'service-key')],text=True).strip()
run(['config','service.recipient',service]);before=len(list((root/'archive/keys').glob('*.json')))
run(['remote','mode','archive','service'])
check(before==len(list((root/'archive/keys').glob('*.json'))),'opening to service does not rotate')
check(subprocess.run([probe,'opens',str(root/'service-key'),str(newprefix/'slots/service.age')],capture_output=True).returncode==0,'configured service identity opens its ordinary slot')
refuse(['overlay','mode','visible','public'],'service project refuses public overlay',words=['project mode service'])
run(['overlay','mode','visible','service']);run(['push','archive','-q'])
run(['remote','mode','archive','public']);check(before==len(list((root/'archive/keys').glob('*.json'))),'opening to public does not rotate')
check(json.loads(run(['overlay','show','visible','--json']).stdout)['access_mode']=='service','opening the project keeps stricter overlay mode')
refuse(['overlay','mode','hidden-rotation-731','public'],'hidden overlay always remains private',words=['always private'])
run(['overlay','mode','visible','public']);run(['push','archive','-q'])
run(['clone','file://'+str(root/'archive'),'project','--public','-q'],P);P['LAYR_PROJECT']='project'
check('after removal' in run(['--line','main','show',second+':after.txt'],P).stdout,'public current epoch clone reads project history')
check(json.loads(run(['overlay','show','visible','--json'],P).stdout)['access_mode']=='public','public overlay is imported only after explicit opening')
public_line=root/'anonymous/store/projects/project/lines/main'
check((public_line/'visible-layer.txt').read_text()=='explicit public overlay bytes\n' and not (public_line/'hidden-layer.txt').exists(),'explicit public overlay files materialize while hidden files stay absent')
check('hidden-rotation-731' not in json.dumps(records(root/'anonymous/store/projects/project')),'public records contain no hidden overlay name')
Q=envs['upgrade'];run(['clone','file://'+str(root/'archive'),'project','--public','-q'],Q);Q['LAYR_PROJECT']='project'
q=json.loads(run(['remote','self','--json'],Q).stdout)
run(['remote','trust','archive',q['id'],'--public-key',q['public_key']]);run(['push','archive','-q']);run(['fetch','origin','-q'],Q)
check(run(['config','cloud.remote.origin.public'],Q).stdout.strip()=='false','a current-epoch machine slot upgrades a public clone without changing the root pin')
run(['remote','mode','archive','service']);service_epoch=latest();service_prefix=root/'archive/project/epochs'/service_epoch['data']['epoch']
check(service_epoch['data']['mode']=='service' and not (service_prefix/'public.json').exists(),'closing public to service rotates without a public opener')
check(json.loads(run(['overlay','show','visible','--json']).stdout)['access_mode']=='service','project closing tightens visible overlay in the same operation')
check(json.loads(run(['overlay','show','hidden-rotation-731','--json']).stdout)['access_mode']=='private','project closing preserves hidden private mode')
run(['remote','public','archive','off']);closed=latest();closed_prefix=root/'archive/project/epochs'/closed['data']['epoch']
check(closed['data']['mode']=='private' and not (closed_prefix/'slots/service.age').exists() and not (closed_prefix/'public.json').exists(),'public off closes to private and removes service/public audience')
check(json.loads(run(['overlay','show','visible','--json']).stdout)['access_mode']=='private','public off tightens service overlay to private')
third=save('private.txt','private after closing\n');run(['push','archive','-q'])
refuse(['fetch','origin','-q'],'anonymous reader receives nothing after closing',P,words=['current project epoch'])
check('after removal' in run(['--line','main','show',second+':after.txt'],P).stdout,'former public reader retains earlier data')
run(['fetch','origin','-q'],C);check('private after closing' in run(['--line','main','show',third+':private.txt'],C).stdout,'remaining reader reads post-closure state')
applied=json.loads(run(['requests','apply','--remote','archive','--json']).stdout)
check(next(r for r in applied if r['id']==request['id'])['status']=='applied','current executor decrypts and applies a request from an earlier sealed epoch')
check((closed_prefix/'claims'/(request['id']+'.json')).is_file(),'executor claim uses the current epoch claims namespace')
# Browse the rotated store through its actual WebDAV caller.
log=open(root/'mount.log','w+')
process=subprocess.Popen([binary,'mount','file://'+str(root/'archive'),'--listen','127.0.0.1:0'],env=dict(C,XDG_CONFIG_HOME=str(root/'mount-config'),XDG_CACHE_HOME=str(root/'mount-cache')),cwd=root,stdout=log,stderr=log)
try:
 for _ in range(400):
  log.seek(0);text=log.read();match=re.search(r'http://127\.0\.0\.1:(\d+)/',text)
  if match:break
  assert process.poll() is None,text
  time.sleep(.025)
 else:raise AssertionError('rotated mount did not start')
 connection=http.client.HTTPConnection('127.0.0.1',int(match[1]),timeout=10);connection.request('GET','/main/private.txt');response=connection.getresponse();data=response.read();connection.close()
 check(response.status==200 and data==b'private after closing\n',f'browse reads current files through rotated epochs (HTTP {response.status}, {data!r})')
finally:process.terminate();process.wait(timeout=10);log.close()
check(not any('hidden-rotation-731' in json.dumps(r) for r in records(project)),'parent records contain no hidden mode metadata')
refuse(['remote','rotate','archive','--remove','machine:'+a['id']],'rotation cannot remove its own remaining signer')
print(f'{checks} passed, 0 failed',flush=True)
PY
