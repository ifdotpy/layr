#!/bin/bash
# Public projects reuse sealed slots; public readers never become writers or overlay readers.
. "$(dirname "$0")/lib.sh"
set -e
for machine_root in writer reader automatic untrusted; do new_fs "$T/$machine_root" 2G; done
for user in layr-public-admin layr-public-writer layr-public-reader; do id "$user" >/dev/null 2>&1 || useradd -m "$user"; done
export PUBLIC_TEST_ROOT="$T" PUBLIC_TEST_BINARY="$LAYR"
python3 - <<'PY'
import hashlib,http.client,http.server,json,os,pathlib,re,shutil,sqlite3,ssl,subprocess,threading,time,urllib.parse
root=pathlib.Path(os.environ['PUBLIC_TEST_ROOT']);binary=os.environ['PUBLIC_TEST_BINARY']
base=os.environ.copy()
for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_PERSON_KEY_FILE','LAYR_PAPER_KEY_FILE','LAYR_GATEWAY_TOKEN_FILE']:
 base.pop(key,None)
A=dict(base,LAYR_PROJECT='project')
W=dict(base,LAYR_ROOT=str(root/'writer/store'))
P=dict(base,LAYR_ROOT=str(root/'reader/store'))
Q=dict(base,LAYR_ROOT=str(root/'automatic/store'))
U=dict(base,LAYR_ROOT=str(root/'untrusted/store'))
checks=0;hidden='hidden-saffron-927'
def run(args,env=A,success=True,input=None):
 p=subprocess.run([binary,*args],env=env,cwd=root,text=True,input=input,capture_output=True,timeout=120)
 if success: assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
 return p
def check(value,label):
 global checks
 assert value,label
 checks+=1;print('PASS',label,flush=True)
def refused(args,env,label,words=(),input=None):
 p=run(args,env,False,input)
 check(p.returncode!=0 and all(word in p.stderr for word in words),label)
 return p
def hashes():
 return {str(p.relative_to(root/'archive')):hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'archive').rglob('*') if p.is_file()}
def records(project):
 db=sqlite3.connect((project/'meta/db/layr.sqlite').as_uri()+'?mode=ro',uri=True)
 try: return [json.loads(raw)['r'] for raw, in db.execute('select raw from records')]
 finally: db.close()
def clean(text,ids): return all(value not in text for value in [hidden,*ids])
a=json.loads(run(['remote','self','--json']).stdout)
w=json.loads(run(['remote','self','--json'],W).stdout)
run(['init','project','-q'])
project=pathlib.Path(A['LAYR_ROOT'])/'projects/project';line=project/'lines/main'
(line/'public.txt').write_text('public version one\n')
(line/'build.sh').write_text('test "$(cat public.txt)" = "public version one"\n')
run(['--line','main','add','public.txt','build.sh']);run(['--line','main','save','-qm','Shared one'])
run(['access','grant','layr-public-admin','admin']);run(['access','grant','layr-public-writer','writer']);run(['access','grant','layr-public-reader','reader'])
run(['remote','add','archive','file://'+str(root/'archive'),'--paper-key-out',str(root/'paper')])
run(['remote','trust','archive',w['id'],'--public-key',w['public_key']])
run(['--line','main','overlay','create','private','--readers',f"machine:{a['id']}",'-q'])
(line/'private.txt').write_text('private overlay bytes\n')
run(['--line','main','overlay','move','private','private.txt','-q']);run(['--line','main','save','-qm','Private state'])
run(['--line','main','overlay','create',hidden,'--hidden','--readers',f"machine:{a['id']}",'-q'])
(line/'hidden.txt').write_text('hidden overlay bytes\n')
run(['--line','main','overlay','move',hidden,'hidden.txt','-q']);run(['--line','main','save','-qm','Hidden state'])
run(['--line','main','view','save','--json'])
run(['--line','main','line','new','topic','-q'])
run(['push','archive','-q'])
overlay=project/'meta/overlays'/hidden;hidden_records=records(overlay)
ids={r['project'] for r in hidden_records};ids.update(s['id'] for r in hidden_records for s in r.get('states',[]))
before=hashes()
refused(['clone','file://'+str(root/'archive'),'private','--public','-q'],P,'explicit public clone refuses a private store',['not public'])
check(before==hashes(),'refused private clone writes no remote objects')
refused(['remote','public','archive','on'],A,'typed confirmation must exactly match the project name',['did not match'],input='wrong\n')
check(before==hashes(),'failed typed confirmation changes nothing')
refused(['remote','public','archive','on'],A,'EOF is not confirmation',['did not match'],input='')
run(['clone','file://'+str(root/'archive'),'project','-q'],W)
W['LAYR_PROJECT']='project'
# Access policy is local to each machine, so grant the writer machine its local admin.
run(['access','grant','layr-public-admin','admin'],W)
run(['push','origin','-q'],W)
# A trusted writer other than the creator can publish even after the creator slot is gone.
(root/f"archive/slots/machine-{a['id']}.age").unlink()
result=run(['remote','public','origin','on','--json'],W,input='project\n')
check(json.loads(result.stdout)['created'] and 'current and future' in result.stderr,'a current admin on a trusted writer machine publishes with a warning')
manifest_path=root/'archive/public.json';slot_path=root/'archive/slots/public.age'
manifest=json.loads(manifest_path.read_bytes());manifest_bytes=manifest_path.read_bytes();slot_bytes=slot_path.read_bytes()
check(set(manifest)=={'v','project','recipient','identity','machine','public_key','sig'} and manifest['v']==1,'public manifest uses the documented signed schema')
check(manifest['machine']==w['id'] and manifest['public_key']==w['public_key'],'public manifest is signed by the publishing writer machine')
check(slot_bytes.startswith(b'age-encryption.org/v1'),'the public slot is age encrypted')
check(not list((root/'archive/overlays').rglob('public.age')) and not list((root/'archive/overlays').rglob('public.json')),'no overlay receives a public slot or manifest')
check(all(b'AGE-SECRET-KEY-' not in p.read_bytes() for p in (root/'archive').rglob('*') if p.is_file() and p!=manifest_path),'the vault identity is absent from all remote plaintext')
check(all(hidden.encode() not in p.read_bytes() for p in (root/'archive').rglob('*') if p.is_file()),'remote bytes contain no hidden overlay name')
before=hashes()
again=run(['remote','public','origin','on','--json'],W,input='project\n')
check(not json.loads(again.stdout)['created'] and before==hashes(),'repeated publication preserves every remote object')
# Recover the publication boundary: a persisted manifest with no slot is retryable.
slot_path.unlink()
run(['remote','public','origin','on'],W,input='project\n')
check(manifest_path.read_bytes()==manifest_bytes and slot_path.read_bytes().startswith(b'age-encryption.org/v1'),'publication resumes from the existing signed opener')
slot_bytes=slot_path.read_bytes()
before=hashes()
refused(['remote','public','origin','off'],W,'off names missing key rotation and says the project stays public',['key rotation','stays public'])
check(before==hashes(),'off changes no remote object')
for env,name,flags in [(P,'project',['--public']),(Q,'project',[])]:
 result=run(['clone','file://'+str(root/'archive'),name,*flags,'--json'],env)
 check(clean(result.stdout+result.stderr,ids),'public clone report has no hidden trace: '+('explicit' if flags else 'automatic'))
 copy=pathlib.Path(env['LAYR_ROOT'])/'projects'/name
 check((copy/'lines/main/public.txt').read_text()=='public version one\n' and (copy/'lines/topic/public.txt').is_file(),'public clone reads files on every line: '+('explicit' if flags else 'automatic'))
 check(not (copy/'lines/main/private.txt').exists() and not (copy/'lines/main/hidden.txt').exists(),'public clone gets no overlay files: '+('explicit' if flags else 'automatic'))
 check(not (copy/'meta/overlays'/hidden).exists(),'public clone creates no hidden project: '+('explicit' if flags else 'automatic'))
 check(clean(json.dumps(records(copy)),ids),'public raw records have no hidden trace: '+('explicit' if flags else 'automatic'))
 machine=(pathlib.Path(env['LAYR_ROOT'])/'machine/id').read_text().strip()
 check(not (root/f'archive/slots/machine-{machine}.age').exists(),'public clone creates no machine slot: '+('explicit' if flags else 'automatic'))
 env['LAYR_PROJECT']=name
 check(run(['config','cloud.remote.origin.public'],env).stdout.strip()=='true','public clone records its local read-only remote marker: '+('explicit' if flags else 'automatic'))
 before=hashes()
 refused(['push','origin','--json'],env,'public-only push is refused: '+('explicit' if flags else 'automatic'),['read only'])
 check(before==hashes(),'refused public push writes no remote object: '+('explicit' if flags else 'automatic'))
 refused(['remote','public','origin','on'],env,'local root in a public clone cannot publish a public identity: '+('explicit' if flags else 'automatic'),['trusted writer slot'],input='project\n')
 check(before==hashes(),'refused reader publication writes no remote object: '+('explicit' if flags else 'automatic'))
 refused(['remote','trust','origin',a['id'],'--public-key',a['public_key']],env,'public reader cannot grant a machine slot: '+('explicit' if flags else 'automatic'),['read only'])
 check(before==hashes(),'refused reader grant writes no remote object: '+('explicit' if flags else 'automatic'))
run(['clone','file://'+str(root/'archive'),'public-only','--public','-q'],dict(U,LAYR_PERSON_KEY_FILE='/missing/person-key',LAYR_PAPER_KEY_FILE='/missing/paper-key'))
check(True,'explicit public clone does not load personal or paper credentials')
run(['--line','main','try','head','--','sh','build.sh'],P)
check(True,'a public clone builds through layr try')
for args in [['line','list','--json'],['--line','main','log','--json'],['--line','main','diff','head'],['--line','main','show','head:public.txt'],['overlay','list','--json'],['config','--list','--json'],['op','log','--all','-n','100000','--json'],['fsck'],['--line','main','view','--json']]:
 output=run(args,P)
 check(clean(output.stdout+output.stderr,ids),'public command has no hidden trace: '+' '.join(args))
for args in [['overlay','show',hidden,'--json'],['overlay','log',hidden,'--ops'],['overlay','log',hidden,'--ops','--json']]:
 output=run(args,P,False);missing=run(args,W,False)
 check(output.returncode!=0 and (output.returncode,output.stdout,output.stderr)==(missing.returncode,missing.stdout,missing.stderr),'public hidden query matches an overlay that never existed byte for byte: '+' '.join(args[:-1]))
# Changes after refused off still reach the public reader through ordinary fetch.
wline=pathlib.Path(W['LAYR_ROOT'])/'projects/project/lines/main'
(wline/'future.txt').write_text('future public bytes\n')
run(['--line','main','add','future.txt'],W);run(['--line','main','save','-qm','Shared future'],W);run(['push','origin','-q'],W)
future=run(['--line','main','rev-parse','head'],W).stdout.strip()
run(['fetch','origin','-q'],P)
check('Shared future' in run(['--line','main','log',future],P).stdout,'public fetch reads future signed states after off refuses')
check('future public bytes' in run(['--line','main','show',future+':future.txt'],P).stdout,'public fetch reads future file contents')
# Signed descriptor binding and age-slot checks fail closed through the clone caller.
for field,value in [('v',2),('project',a['id']),('recipient','invalid'),('identity','invalid'),('machine',a['id']),('public_key',a['public_key']),('sig','AAAA')]:
 changed=dict(manifest);changed[field]=value;manifest_path.write_text(json.dumps(changed))
 try:
  refused(['clone','file://'+str(root/'archive'),'tampered','--public','-q'],U,'clone refuses tampered public manifest field: '+field)
 finally: manifest_path.write_bytes(manifest_bytes)
slot_path.write_bytes((root/'archive/slots/paper.age').read_bytes())
try: refused(['clone','file://'+str(root/'archive'),'bad-slot','--public','-q'],U,'clone refuses a public slot that the opener cannot decrypt',['public slot'])
finally: slot_path.write_bytes(slot_bytes)
# Browse actual WebDAV bytes without a machine, person or paper slot.
log=open(root/'mount.log','w+')
process=subprocess.Popen([binary,'mount','file://'+str(root/'archive'),'--listen','127.0.0.1:0'],env=dict(Q,XDG_CONFIG_HOME=str(root/'mount-config'),XDG_CACHE_HOME=str(root/'mount-cache')),cwd=root,stdout=log,stderr=log)
try:
 for _ in range(400):
  log.seek(0);text=log.read();match=re.search(r'http://127\.0\.0\.1:(\d+)/',text)
  if match: break
  assert process.poll() is None,text
  time.sleep(.025)
 else: raise AssertionError('public mount did not start')
 def get(path):
  connection=http.client.HTTPConnection('127.0.0.1',int(match[1]),timeout=10)
  connection.request('GET',path);response=connection.getresponse();data=response.read();connection.close()
  return response.status,data
 check(get('/main/public.txt')==(200,b'public version one\n'),'public mount reads shared file bytes')
 check(get('/main/future.txt')==(200,b'future public bytes\n'),'public mount reads future file bytes')
 check(get('/main/private.txt')[0]==404 and get('/main/hidden.txt')[0]==404,'public mount exposes no overlay files')
finally: process.terminate();process.wait(timeout=10);log.close()
# An ordinary TLS file server serves only immutable public objects and listings at /p/.
def openssl(args): subprocess.run(['openssl',*args],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-ca-key.pem'),'-out',str(root/'tls-ca.pem'),'-days','1','-subj','/CN=Public test CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign'])
openssl(['req','-new','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-key.pem'),'-out',str(root/'tls.csr'),'-subj','/CN=localhost'])
(root/'tls-ext.cnf').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n')
openssl(['x509','-req','-in',str(root/'tls.csr'),'-CA',str(root/'tls-ca.pem'),'-CAkey',str(root/'tls-ca-key.pem'),'-CAcreateserial','-out',str(root/'tls-cert.pem'),'-days','1','-extfile',str(root/'tls-ext.cnf')])
requests=[]
class PublicFiles(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  requests.append((self.command,self.path,self.headers.get('Authorization')))
  parsed=urllib.parse.urlsplit(self.path);prefix='/p/project/'
  if not parsed.path.startswith(prefix): self.send_error(404);return
  name=parsed.path[len(prefix):]
  if name=='objects':
   query=urllib.parse.parse_qs(parsed.query);wanted=query.get('prefix',[''])[0]
   if not wanted.startswith(('records/','states/')): self.send_error(404);return
   body=json.dumps({'objects':[{'key':str(p.relative_to(root/'archive')),'modified':int(p.stat().st_mtime*1000)} for p in (root/'archive').rglob('*') if p.is_file() and str(p.relative_to(root/'archive')).startswith(wanted)]}).encode()
  elif name in ['store.json','public.json','slots/public.age'] or name.startswith(('records/','states/')):
   path=root/'archive'/name
   if '..' in pathlib.PurePosixPath(name).parts or not path.is_file(): self.send_error(404);return
   body=path.read_bytes()
  else: self.send_error(404);return
  self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_PUT(self): requests.append((self.command,self.path,None));self.send_error(405)
 def do_POST(self): requests.append((self.command,self.path,None));self.send_error(405)
 def do_DELETE(self): requests.append((self.command,self.path,None));self.send_error(405)
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),PublicFiles)
context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(root/'tls-cert.pem',root/'tls-key.pem')
server.socket=context.wrap_socket(server.socket,server_side=True)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
network=dict(U,LAYR_GATEWAY_CA_FILE=str(root/'tls-ca.pem'))
url='layr+https://localhost:'+str(server.server_port)+'/project'
try:
 output=run(['clone',url,'network','--public','--json'],network)
 check(clean(output.stdout+output.stderr,ids),'HTTP public clone has no hidden trace')
 nproject=pathlib.Path(network['LAYR_ROOT'])/'projects/network'
 check((nproject/'lines/main/future.txt').read_text()=='future public bytes\n','HTTP public clone reads state and file objects from the /p/ layout')
 network['LAYR_PROJECT']='network'
 run(['fetch','origin','-q'],network)
 check(True,'HTTP public fetch reads through /p/')
 before=hashes();refused(['push','origin','-q'],network,'HTTP public push is refused',['read only'])
 check(before==hashes(),'HTTP public push changes no object')
 check(all(method=='GET' and path.startswith('/p/project/') and auth is None for method,path,auth in requests),'every HTTP public request uses GET /p/project/ with no credential')
 check(any(path.startswith('/p/project/objects?prefix=records') for _,path,_ in requests) and any(path.startswith('/p/project/objects?prefix=states') for _,path,_ in requests),'HTTP public listings use only the documented record and state routes')
 check(not any('/overlays/' in path or '/people/' in path or '/token' in path for _,path,_ in requests),'HTTP public reads request no overlays, people or tokens')
 machine=(pathlib.Path(network['LAYR_ROOT'])/'machine/id').read_text().strip()
 check(not (root/f'archive/slots/machine-{machine}.age').exists(),'HTTP public clone creates no machine slot')
finally: server.shutdown();server.server_close();thread.join(timeout=10)
# Current admin and writer roles are checked using the real daemon caller identity.
for path in [root/'archive',*(root/'archive').rglob('*')]:
 os.chmod(path,0o755 if path.is_dir() else 0o644)
shutil.copy2(binary,root/'layr');os.chmod(root/'layr',0o755);os.chmod(root,0o755)
socket=root/'daemon.sock';daemon_log=open(root/'daemon.log','w')
daemon=subprocess.Popen([binary,'daemon','--socket',str(socket),'--tick','3600'],env=W,cwd=root,stdout=daemon_log,stderr=daemon_log)
try:
 for _ in range(200):
  if socket.exists(): break
  assert daemon.poll() is None
  time.sleep(.025)
 def person(user):
  return subprocess.run(['runuser','-u',user,'--','env','LAYR_SOCKET='+str(socket),'LAYR_PROJECT=project',str(root/'layr'),'remote','public','origin','on'],input='project\n',cwd='/',capture_output=True,text=True,timeout=120)
 before=hashes();result=person('layr-public-writer')
 check(result.returncode!=0 and 'permission denied' in result.stderr and before==hashes(),'a non-admin writer cannot change publication')
 result=person('layr-public-admin')
 check(result.returncode==0,'a current admin can repeat publication on its trusted writer machine: '+str(result.returncode)+' '+result.stderr)
 check(before==hashes(),'non-root admin repeat publication changes no remote object')
finally: daemon.terminate();daemon.wait(timeout=10);daemon_log.close()
# A normal reader can fetch a marked public remote, but cannot fetch a private remote
# or push. Granting its machine an own slot removes that local public marker.
import pwd
run(['config','access',json.dumps({'user:0':'admin','user:'+str(pwd.getpwnam('layr-public-reader').pw_uid):'reader'})],P)
run(['remote','add','private','file://'+str(root/'archive')],P)
socket=root/'reader-daemon.sock';daemon_log=open(root/'reader-daemon.log','w')
daemon=subprocess.Popen([binary,'daemon','--socket',str(socket),'--tick','3600'],env=P,cwd=root,stdout=daemon_log,stderr=daemon_log)
try:
 for _ in range(200):
  if socket.exists(): break
  assert daemon.poll() is None
  time.sleep(.025)
 def reader(args):
  return subprocess.run(['runuser','-u','layr-public-reader','--','env','LAYR_SOCKET='+str(socket),'LAYR_PROJECT=project',str(root/'layr'),*args],cwd='/',capture_output=True,text=True,timeout=120)
 result=reader(['fetch','origin','-q'])
 check(result.returncode==0,'a non-root daemon reader fetches a marked public remote: '+str(result.returncode)+' '+result.stderr)
 result=reader(['fetch','private','-q'])
 check(result.returncode!=0 and 'permission denied' in result.stderr,'an unmarked remote keeps the private fetch policy')
 result=reader(['push','origin','-q'])
 check(result.returncode!=0 and 'permission denied' in result.stderr,'public marking does not relax push policy for a non-root reader')
 p=json.loads(run(['remote','self','--json'],P).stdout)
 run(['remote','trust','origin',p['id'],'--public-key',p['public_key']],W)
 run(['push','origin','-q'],W)
 run(['fetch','origin','-q'],P)
 check(run(['config','cloud.remote.origin.public'],P).stdout.strip()=='false','fetch clears the public marker after the machine receives its own slot')
 result=reader(['fetch','origin','-q'])
 check(result.returncode!=0 and 'permission denied' in result.stderr,'after upgrade the ordinary private fetch policy applies again')
 run(['push','origin','-q'],P)
 check(True,'the newly trusted machine can push under its normal local policy')
finally: daemon.terminate();daemon.wait(timeout=10);daemon_log.close()
(root/'check-count').write_text(str(checks));print('public project checks:',checks)
PY
PASS=$(cat "$T/check-count")
finish
