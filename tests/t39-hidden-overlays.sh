#!/bin/bash
# Hidden overlays have no parent metadata and travel only through reader slots or permitted peers.
. "$(dirname "$0")/lib.sh"
set -e
for machine_root in reader other peer; do new_fs "$T/$machine_root" 2G; done
for user in layr-hidden-reader layr-hidden-other; do id "$user" >/dev/null 2>&1 || useradd -m "$user"; done
export HIDDEN_TEST_ROOT="$T" HIDDEN_TEST_BINARY="$LAYR"
python3 - <<'PY'
import http.client,json,os,pathlib,re,shutil,sqlite3,subprocess,time
root=pathlib.Path(os.environ['HIDDEN_TEST_ROOT'])
binary=os.environ['HIDDEN_TEST_BINARY']
base=os.environ.copy()
for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_PERSON_KEY_FILE','LAYR_PAPER_KEY_FILE']:
 base.pop(key,None)
A=dict(base,LAYR_PROJECT='project')
R=dict(base,LAYR_ROOT=str(root/'reader/store'),LAYR_PERSON_DIR=str(root/'reader-person'),LAYR_PERSON_LOGIN='hidden-reader@example.com')
N=dict(base,LAYR_ROOT=str(root/'other/store'),LAYR_PERSON_DIR=str(root/'other-person'),LAYR_PERSON_LOGIN='hidden-other@example.com')
P=dict(base,LAYR_ROOT=str(root/'peer/store'))
name='hidden-cobalt-731'
checks=0
def run(args,env=A,success=True):
 p=subprocess.run([binary,*args],env=env,cwd=root,text=True,capture_output=True,timeout=120)
 if success: assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
 return p
def check(value,label):
 global checks
 assert value,label
 checks+=1
 print('PASS',label,flush=True)
def machine(env): return json.loads(run(['remote','self','--json'],env).stdout)
def records(path):
 db=sqlite3.connect(path/'meta/db/layr.sqlite')
 try: return [json.loads(line)['r'] for (line,) in db.execute('SELECT raw FROM records')]
 finally: db.close()
def absent(text,ids=()): return name not in text and all(value not in text for value in ids)
a=machine(A);r=machine(R);n=machine(N);p=machine(P)
run(['init','project','-q'])
project=pathlib.Path(A['LAYR_ROOT'])/'projects/project'
line=project/'lines/main'
(line/'public.txt').write_text('shared bytes\n')
run(['--line','main','add','public.txt'])
run(['--line','main','save','-qm','Public file'])
for user in ['layr-hidden-reader','layr-hidden-other']: run(['access','grant',user,'admin'])
run(['remote','add','archive','file://'+str(root/'archive'),'--paper-key-out',str(root/'vault-paper')])
for who in [r,n,p]: run(['remote','trust','archive',who['id'],'--public-key',who['public_key']])
people=[]
for env,label in [(R,'reader'),(N,'other')]:
 key=root/(label+'-paper')
 run(['person','keys','--paper-key-out',str(key)],env)
 env['LAYR_PERSON_KEY_FILE']=str(key)
 person=json.loads(run(['person','show','--json'],env).stdout)
 people.append(person)
 invite=root/(label+'-invite.age')
 run(['remote','grant','archive','--person',env['LAYR_PERSON_LOGIN'],'--recipient',person['recipient'],'--invite-out',str(invite)],dict(A,LAYR_PERSON_LOGIN=env['LAYR_PERSON_LOGIN']))
 run(['person','accept','--invite-file',str(invite)],env)
readers=f"layr-hidden-reader,machine:{a['id']},person:{R['LAYR_PERSON_LOGIN']}={people[0]['recipient']}"
parent_before=len(records(project))
run(['--line','main','overlay','create',name,'--hidden','--readers',readers,'-q'])
check(len(records(project))==parent_before,'hidden creation adds no parent operation')
run(['config',f'overlay.{name}.peers',json.dumps([p['id']])])
check(len(records(project))==parent_before,'hidden peer settings stay in the overlay log')
(line/'hidden.txt').write_text('hidden test bytes\n')
run(['--line','main','overlay','move',name,'hidden.txt','-q'])
run(['--line','main','save','-qm','Hidden state'])
check(len(records(project))==parent_before,'hidden use and save add no parent operation')
view=json.loads(run(['--line','main','view','save','--json']).stdout)
check(len(records(project))==parent_before,'a hidden view descriptor stays in the overlay log')
run(['--line','main','line','new','topic','-q'])
run(['--line','topic','overlay','use','--none','-q'])
check(not any(row['hidden'] is False for row in json.loads(run(['overlay','list','--json']).stdout)),'hidden creation uses follow mode only')
# The name, root, state IDs and overlay ID never enter shared records or settings.
overlay=project/'meta/overlays'/name
own=records(overlay)
ids={row['project'] for row in own}
ids.update(state['id'] for row in own for state in row.get('states',[]))
parent=records(project)
check(absent(json.dumps(parent),ids),'raw parent records contain no hidden name or layer IDs')
config=run(['config','--list','--json']).stdout
check(absent(config,ids),'parent config contains no hidden name or layer IDs')
# Grep all parent database bytes, including the WAL, for metadata left by any write.
for path in (project/'meta/db').iterdir():
 if path.is_file():
  check(all(value.encode() not in path.read_bytes() for value in [name,*ids]),'raw shared database excludes hidden metadata: '+path.name)
report=json.loads(run(['push','archive','--json']).stdout)
check(any(name in row for row in report['overlays']),'the reader push report names its hidden overlay')
check(absent(json.dumps(records(project)),ids),'parent push and sync records contain no hidden name, count or layer ID')
check(not any((root/'archive').glob('overlays/*/keys/*.json')),'hidden key manifests have no plaintext key record')
check(any((root/'archive').glob('overlays/*/keys/*.age')),'hidden key manifests use overlay encryption')
check(all(name.encode() not in file.read_bytes() for file in (root/'archive').rglob('*') if file.is_file()),'grep finds no hidden name anywhere in the remote store')
report=json.loads(run(['clone','file://'+str(root/'archive'),'project','--json'],R).stdout)
rproject=pathlib.Path(R['LAYR_ROOT'])/'projects/project'
check(any(name in row for row in report['overlays']),'the reader person discovers the hidden overlay through its slot')
check((rproject/'lines/main/hidden.txt').read_text()=='hidden test bytes\n','the reader clone follows hidden overlay records')
check(not (rproject/'lines/topic/hidden.txt').exists(),'a disabled hidden overlay stays disabled after clone')
check(name in run(['--project','project','overlay','list'],R).stdout,'the reader lists the hidden overlay')
check('Hidden state' in run(['--project','project','--line','main','overlay','log',name],R).stdout,'the reader sees the hidden overlay log')
report=run(['clone','file://'+str(root/'archive'),'project','--json'],N)
check(absent(report.stdout,ids),'the non-reader clone report has no hidden trace')
nproject=pathlib.Path(N['LAYR_ROOT'])/'projects/project'
check(not (nproject/'meta/overlays'/name).exists(),'the non-reader gets no hidden project')
check(not (nproject/'lines/main/hidden.txt').exists(),'the non-reader gets shared files only')
def mount_read(env):
 logfile=open(root/('mount-'+pathlib.Path(env['LAYR_ROOT']).parent.name+'.log'),'w+')
 process=subprocess.Popen([binary,'mount','file://'+str(root/'archive'),'--listen','127.0.0.1:0'],env=env,cwd=root,stdout=logfile,stderr=logfile)
 try:
  for _ in range(400):
   logfile.seek(0);text=logfile.read();match=re.search(r'http://127\.0\.0\.1:(\d+)/',text)
   if match: break
   assert process.poll() is None,text
   time.sleep(.025)
  else: raise AssertionError('mount did not start')
  connection=http.client.HTTPConnection('127.0.0.1',int(match[1]),timeout=10)
  connection.request('GET','/main/hidden.txt');response=connection.getresponse();data=response.read();connection.close()
  return response.status,data
 finally:
  process.terminate();process.wait(timeout=10);logfile.close()
check(mount_read(R)==(200,b'hidden test bytes\n'),'the reader mount follows the hidden overlay')
check(mount_read(N)[0]==404,'the non-reader mount has no hidden file')

for args in [['overlay','list'],['overlay','list','--json'],['--line','main','overlay','use'],['config','--list','--json'],['op','log','--all','-n','100000','--json'],['--line','main','log','--json'],['fsck'],['push','origin','--json']]:
 result=run(['--project','project',*args],N)
 check(absent(result.stdout+result.stderr,ids),'the non-reader output has no hidden trace: '+' '.join(args))
check(absent(json.dumps(records(nproject)),ids),'the non-reader raw shared records have no hidden trace')
# Peer offer metadata also excludes the hidden name when the target is not allowed.
run(['remote','add','blocked','ssh://test/project'])
blocked=json.loads(run(['sync','blocked','--json','--command','--','env','LAYR_ROOT='+N['LAYR_ROOT'],binary,'serve-peer','project']).stdout)
check(not (nproject/'meta/overlays'/name).exists(),'an excluded peer receives no hidden records')
run(['--project','project','remote','add','source','ssh://test/project'],N)
result=run(['--project','project','fetch','source','--json','--command','--','env','LAYR_ROOT='+A['LAYR_ROOT'],binary,'serve-peer','project'],N)
check(absent(result.stdout+result.stderr,ids),'an excluded peer receives no hidden omission name')
run(['remote','add','allowed','ssh://test/project'])
run(['sync','allowed','--command','--','env','LAYR_ROOT='+P['LAYR_ROOT'],binary,'serve-peer','project'])
check((pathlib.Path(P['LAYR_ROOT'])/'projects/project/meta/overlays'/name/'meta/db/layr.sqlite').exists(),'an explicitly allowed peer receives the hidden project')
# Two local people on a service, including an admin who is not an overlay reader.
shutil.copy2(binary,root/'layr');os.chmod(root/'layr',0o755);os.chmod(root,0o755)
# Both people have store access. The reader slot controls hidden overlay discovery.
for path in [root/'archive',*(root/'archive').rglob('*')]:
 os.chmod(path,0o777 if path.is_dir() else 0o644)
socket=root/'daemon.sock';log=open(root/'daemon.log','w')
daemon=subprocess.Popen([binary,'daemon','--socket',str(socket),'--tick','3600'],env=A,cwd=root,stdout=log,stderr=log)
def person(user,args,success=True):
 out=subprocess.run(['runuser','-u',user,'--','env','LAYR_SOCKET='+str(socket),'LAYR_PROJECT=project','LAYR_LINE=main',str(root/'layr'),*args],cwd='/',capture_output=True,text=True,timeout=120)
 if success: assert out.returncode==0,(user,args,out.stdout,out.stderr)
 return out
try:
 for _ in range(200):
  if socket.exists(): break
  assert daemon.poll() is None
  time.sleep(.025)
 for args in [['overlay','list'],['overlay','list','--json'],['overlay','use'],['config','--list','--json'],['op','log','--all','-n','100000','--json'],['log','--json'],['fsck'],['push','archive','--json']]:
  result=person('layr-hidden-other',args)
  check(absent(result.stdout+result.stderr,ids),'a non-reader admin has no hidden trace: '+' '.join(args))

 # Compare the same line with and without its hidden overlay, including exit status and errors.
 commands=[['status'],['status','--json'],['diff'],['diff','--cached'],['diff','head'],
  ['grep','shared'],['grep','hidden'],['grep','--cached','hidden'],['ls-files'],['ls-files','--stage'],
  ['ls-files','--others'],['show',':public.txt'],['show',':hidden.txt'],['view','--json']]
 run(['--line','main','overlay','use','--none','-q'])
 baseline=[]
 for args in commands:
  output=person('layr-hidden-other',args,False)
  baseline.append((output.returncode,output.stdout,output.stderr))
 run(['--line','main','overlay','use',name,'-q'])
 for args,expected in zip(commands,baseline):
  output=person('layr-hidden-other',args,False)
  check((output.returncode,output.stdout,output.stderr)==expected,'non-reader output matches no hidden overlay byte for byte: '+' '.join(args))
 direct=subprocess.run(['runuser','-u','layr-hidden-other','--','cat',str(line/'hidden.txt')],cwd='/',capture_output=True)
 check((line.stat().st_mode & 0o777)==0o700,'the working folder uses the existing owner-only permission mode')
 check(direct.returncode!=0 and b'hidden test bytes' not in direct.stdout,'the non-reader cannot read the owner working file through its path')
 check((line/'hidden.txt').read_text()=='hidden test bytes\n','the owner working folder still holds the hidden file')
 check(name in person('layr-hidden-reader',['overlay','list']).stdout,'the reader person sees the hidden row')
 check('hidden' in person('layr-hidden-reader',['overlay','list']).stdout,'the reader row has the hidden badge')
 check('Hidden state' in person('layr-hidden-reader',['overlay','log',name]).stdout,'the reader person sees overlay history')
 # Read-only queries must not change the shared operation count or add a hidden marker.
 check(absent(json.dumps(records(project)),ids),'service requests leave no hidden metadata in the parent log')
 # A new person reader discovers it after the next push.
 run(['overlay','readers',name,readers,f"person:{N['LAYR_PERSON_LOGIN']}={people[1]['recipient']}"])
 run(['push','archive','-q'])
 run(['--project','project','fetch','origin','-q'],N)
 check((nproject/'meta/overlays'/name/'meta/db/layr.sqlite').exists(),'a new person reader discovers the overlay after push')
 run(['overlay','show-to-project',name])
 check(any(row['op']=='overlay.show-to-project' for row in records(overlay)),'unhide is recorded in the overlay log')
 before=(len(records(project)),len(records(overlay)))
 run(['overlay','show-to-project',name])
 check(before==(len(records(project)),len(records(overlay))),'a repeated unhide adds no duplicate operation')
 check(any(row['op']=='overlay.create' and row['data']['name']==name for row in records(project)),'unhide publishes the normal public creation')
 check(name in person('layr-hidden-other',['overlay','list']).stdout,'the non-reader sees the overlay after unhide')
 check(not json.loads(person('layr-hidden-other',['overlay','list','--json']).stdout)[0]['hidden'],'unhide removes the hidden badge')
 check(run(['overlay','create',name,'--hidden'],success=False).returncode!=0,'a visible overlay cannot be recreated as hidden')
 check(run(['config',f'overlay.{name}.hidden','true'],success=False).returncode!=0,'config cannot hide a visible overlay')
 check(run(['overlay','hide',name],success=False).returncode!=0,'hide after show is refused')
 run(['push','archive','-q'])
 check(any((root/'archive').glob('overlays/*/keys/*.json')),'unhide publishes a public signed key record')
finally:
 daemon.terminate();daemon.wait(timeout=10);log.close()
(root/'check-count').write_text(str(checks))
print('hidden overlay checks:',checks)
PY
PASS=$(cat "$T/check-count")
finish
