#!/bin/bash
# Project epochs, access modes, audience closure and overlay privacy through the CLI.
. "$(dirname "$0")/lib.sh"
set -e
for machine in owner removed reader fresh anonymous person; do new_fs "$T/$machine" 2G; done
export ROT_TEST_ROOT="$T" ROT_TEST_BINARY="$LAYR"
export ROT_TEST_PROBE="$(dirname "$LAYR")/examples/epoch_probe"
python3 - <<'PY'
import hashlib,json,os,pathlib,sqlite3,subprocess
root=pathlib.Path(os.environ['ROT_TEST_ROOT']);binary=os.environ['ROT_TEST_BINARY'];probe=os.environ['ROT_TEST_PROBE'];checks=0
base=os.environ.copy()
for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_PERSON_KEY_FILE','LAYR_PAPER_KEY_FILE','LAYR_GATEWAY_TOKEN_FILE']:base.pop(key,None)
envs={n:dict(base,LAYR_ROOT=str(root/n/'store')) for n in ['owner','removed','reader','fresh','anonymous','person']}
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
run(['fetch','origin','-q'],C)
check('after removal' in run(['--line','main','show',second+':after.txt'],C).stdout,'remaining reader fetches the new epoch')
# Paper slot starts a new machine and opens all old objects without rekeying them.
D['LAYR_PAPER_KEY_FILE']=str(root/'paper');run(['clone','file://'+str(root/'archive'),'project','-q'],D);D['LAYR_PROJECT']='project'
check('epoch zero' in run(['--line','main','show',first+':shared.txt'],D).stdout,'fresh reader reads sealed earlier history')
check('after removal' in run(['--line','main','show',second+':after.txt'],D).stdout,'fresh reader reads current history')
check(all(hashes().get(k)==v for k,v in old_hashes.items()),'rotation never rewrites old objects')
run(['remote','rotate','archive','--reason','second rotation']);new=latest();newprefix=root/'archive/project/epochs'/new['data']['epoch']
check((newprefix/'history/root.age').is_file() and (newprefix/('history/'+epoch['data']['epoch']+'.age')).is_file(),'second rotation seals every earlier identity')
run(['fetch','origin','-q'],C);check(True,'remaining reader follows two rotations')
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
check(not any('hidden-rotation-731' in json.dumps(r) for r in records(project)),'parent records contain no hidden mode metadata')
refuse(['remote','rotate','archive','--remove','machine:'+a['id']],'rotation cannot remove its own remaining signer')
print(f'{checks} passed, 0 failed',flush=True)
PY
