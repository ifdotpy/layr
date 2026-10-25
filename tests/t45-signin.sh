#!/bin/bash
# Public gateway person sessions use the person key, including the real CLI callers.
. "$(dirname "$0")/lib.sh"
set -e
export SIGNIN_TEST_ROOT="$T" SIGNIN_TEST_BINARY="$LAYR"
python3 - <<'PY'
import base64,hashlib,http.server,json,os,pathlib,secrets,ssl,subprocess,threading,time,urllib.parse
root=pathlib.Path(os.environ['SIGNIN_TEST_ROOT']); binary=os.environ['SIGNIN_TEST_BINARY']; base=os.environ.copy()
for name in ['LAYR_PERSON_KEY_FILE','LAYR_GATEWAY_TOKEN_FILE','LAYR_PAPER_KEY_FILE']:
 base.pop(name,None)
checks=0
def check(value,label):
 global checks
 assert value,label
 checks+=1; print('PASS',label,flush=True)
def run(args,env,success=True):
 cwd=pathlib.Path(env['LAYR_ROOT'])/'projects'/env['LAYR_PROJECT']/'lines'/env.get('LAYR_LINE','main') if env.get('LAYR_PROJECT') else root
 p=subprocess.run([binary,*args],env=env,cwd=cwd,text=True,capture_output=True,timeout=45)
 if success: assert p.returncode==0,(args,p.stderr)
 return p
def openssl(args): subprocess.run(['openssl',*args],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-ca-key.pem'),'-out',str(root/'tls-ca.pem'),'-days','1','-subj','/CN=Signin test CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign'])
openssl(['req','-new','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-key.pem'),'-out',str(root/'tls.csr'),'-subj','/CN=localhost'])
(root/'tls-ext.cnf').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n')
openssl(['x509','-req','-in',str(root/'tls.csr'),'-CA',str(root/'tls-ca.pem'),'-CAkey',str(root/'tls-ca-key.pem'),'-CAcreateserial','-out',str(root/'tls-cert.pem'),'-days','1','-extfile',str(root/'tls-ext.cnf')])
people={}; challenges={}; sessions={}; invites={'registration-invite':'ann@example.com','fresh-invite':'fresh@example.com'}; stats={'challenge':0,'session':0,'registration':0,'renew':0,'requests':0,'index':0,'inbox':0,'slots':0,'request_attempts':0,'epoch_requests':0,'epoch_claims':0,'epoch_proofs':0}; faults={}; errors=[]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def reply(self,status,body):
  data=body if isinstance(body,bytes) else json.dumps(body).encode();self.send_response(status);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
 def do_GET(self): self.handle_api()
 def do_POST(self): self.handle_api()
 def do_PUT(self): self.handle_api()
 def do_DELETE(self): self.handle_api()
 def handle_api(self):
  try: self.api()
  except Exception as e:
   errors.append(type(e).__name__);self.reply(500,{})
 def api(self):
  parsed=urllib.parse.urlsplit(self.path); path=parsed.path; login=urllib.parse.parse_qs(parsed.query).get('login',[''])[0]
  raw=self.rfile.read(int(self.headers.get('Content-Length','0')));body=json.loads(raw or b'{}')
  bearer=self.headers.get('Authorization','').removeprefix('Bearer ')
  if path=='/api/session/challenge' and self.command=='GET':
   assert not bearer
   stats['challenge']+=1; challenge=base64.b64encode(os.urandom(32)).decode(); challenges[challenge]=login
   self.reply(200,{'challenge':challenge,'expires':int(time.time()*1000)+(-1000 if faults.pop('expired_challenge',False) else 60000)});return
  if path=='/api/session' and self.command=='POST':
   stats['session']+=1;login=body['login'];challenge=body['challenge']; signature=body['signature']
   assert signature==signature.lower() and len(signature)==128
   bound=challenges.pop(challenge,None)
   key=people.get(login,{}).get('signing_key')
   if not key or bound!=login: self.reply(401,{});return
   (root/'verify.der').write_bytes(bytes.fromhex('302a300506032b6570032100'+key))
   (root/'verify.sig').write_bytes(bytes.fromhex(signature))
   (root/'verify.message').write_bytes(b'layr/session/v1\0'+login.encode()+b'\0'+base64.b64decode(challenge))
   verified=subprocess.run(['openssl','pkeyutl','-verify','-pubin','-inkey',str(root/'verify.der'),'-keyform','DER','-rawin','-in',str(root/'verify.message'),'-sigfile',str(root/'verify.sig')],capture_output=True).returncode==0
   if not verified: self.reply(401,{});return
   token=base64.b64encode(os.urandom(32)).decode();sessions[token]=login
   self.reply(200,{'token':token,'expires':int(time.time()*1000)+(1000 if faults.pop('short_session',False) else 43200000),'login':login});return
  if path=='/api/person' and self.command=='PUT':
   if invites.get(bearer)!=login: self.reply(401,{});return
   if login in people: self.reply(409,{});return
   assert all(body.get(k) for k in ['recipient','paper','signing_key','signing'])
   invites.pop(bearer);people[login]=body;stats['registration']+=1;self.reply(200,{'login':login,'new':True});return
  login=sessions.get(bearer)
  # Project data keeps its read/write capability; person exchanges use the session.
  if path.startswith('/v1/p/project/objects') and bearer in ['v1.read.signinTest','v1.write.signinTest']:
   login='ann@example.com'
  if not login: self.reply(401,{});return
  if faults.pop('reject_session',False):
   sessions.pop(bearer);stats['renew']+=1;self.reply(401,{});return
  if faults.get('always_reject',False): self.reply(401,{});return
  if path=='/api/person':
   self.reply(200,dict(people[login],login=login,index=people[login].get('index'),inbox=people[login].get('inbox',[])));return
  if path=='/api/projects': self.reply(200,{'projects':['project']});return
  if path=='/api/person/index' and self.command=='PUT':
   assert body['previous'] is None;people[login]['index']=body['data'];stats['index']+=1;self.reply(200,{});return
  if path=='/api/person/inbox/1-test' and self.command=='DELETE':
   people[login]['inbox']=[];stats['inbox']+=1;self.reply(200,{});return
  if path=='/v1/p/project/store.json': self.reply(200,(root/'archive/store.json').read_bytes());return
  scope=root/'archive'
  epoch=urllib.parse.parse_qs(parsed.query).get('epoch',['root'])[0]
  if epoch!='root': scope=scope/'project/epochs'/epoch
  if path=='/v1/p/project/challenge':
   assert bearer in sessions
   stats['epoch_proofs']+=int(epoch!='root')
   self.reply(200,{'challenge':base64.b64encode(next((scope/'records').rglob('*.age')).read_bytes()).decode()});return
  if path=='/v1/p/project/token':
   assert bearer in sessions and '"machine"' in body['challenge']
   self.reply(200,{'token':'v1.read.signinTest','expires':int(time.time()*1000)+60000});return
  if path=='/v1/p/project/slots':
   self.reply(200,{'slots':[{'name':p.stem,'data':base64.b64encode(p.read_bytes()).decode()} for p in (scope/'slots').glob('*.age')]});return
  if path.startswith('/v1/p/project/slots/'):
   name=path.rsplit('/',1)[1];assert name.startswith('person-'+login+'-');stats['slots']+=1
   self.reply(200,(root/'archive/slots'/name).read_bytes());return
  object_key=path.removeprefix('/v1/p/project/objects/')
  logical=object_key.split('/',3)[3] if object_key.startswith('project/epochs/') else object_key
  if path.startswith('/v1/p/project/objects/') and logical.startswith('requests/') and self.command=='PUT':
   assert bearer in sessions
   stats['request_attempts']+=1
   if faults.pop('reject_request_session',False):
    sessions.pop(bearer);self.reply(401,{});return
   assert body['v']==2 and body['person']==login and body['sig']
   if object_key.startswith('project/epochs/'):
    assert body['epoch']==object_key.split('/')[2]
    target=root/'archive'/object_key;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(raw)
    stats['epoch_requests']+=1
   stats['requests']+=1;self.reply(201,{});return
  if path=='/v1/p/project/objects':
   prefix=urllib.parse.parse_qs(parsed.query).get('prefix',[''])[0]
   self.reply(200,{'objects':[{'key':str(p.relative_to(root/'archive')),'modified':int(p.stat().st_mtime*1000)} for p in (root/'archive').rglob('*') if p.is_file() and str(p.relative_to(root/'archive')).startswith(prefix)]});return
  if path.startswith('/v1/p/project/objects/'):
   target=root/'archive'/object_key
   if self.command=='GET':
    self.reply(200,target.read_bytes()) if target.is_file() else self.reply(404,{})
    return
   if self.command=='PUT' and logical.startswith('claims/'):
    assert bearer=='v1.write.signinTest' and body['s'] and body['r']['request']==logical.removeprefix('claims/').removesuffix('.json')
    if target.exists(): self.reply(412,{});return
    target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(raw);stats['epoch_claims']+=1;self.reply(201,{});return
  self.reply(404,{})
server=http.server.HTTPServer(('127.0.0.1',0),Handler)
context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(root/'tls-cert.pem',root/'tls-key.pem');server.socket=context.wrap_socket(server.socket,server_side=True)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();site='https://localhost:'+str(server.server_port)
ann=dict(base,LAYR_PERSON_DIR=str(root/'ann'),LAYR_PERSON_LOGIN='ann@example.com',LAYR_GATEWAY_CA_FILE=str(root/'tls-ca.pem'))
try:
 p=run(['person','keys','--site',site,'--invite','invalid-invite','--paper-key-out',str(root/'ann.paper')],ann,False)
 check(p.returncode!=0 and '401' in p.stderr and stats['registration']==0,('invalid invite refuses first registration',p.returncode,p.stderr))
 ann['LAYR_PERSON_KEY_FILE']=str(root/'ann.paper')
 p=run(['person','keys','--site',site,'--invite','registration-invite'],ann)
 check(stats['registration']==1 and stats['session']==1,'invite registration signs in with the new person key')
 check(stats['challenge']==1 and 'registration-invite' not in p.stdout+p.stderr,'registration signs exactly one fresh challenge without exposing invite')
 before=stats['session'];run(['person','keys','--site',site],ann)
 check(stats['registration']==1 and stats['session']==before+1,'repeated keys verifies existing registration without overwriting it')
 shown=json.loads(run(['person','show','--site',site,'--json'],ann).stdout)
 check(shown['login']=='ann@example.com','person show authenticates without Tailscale')
 saved=dict(ann);saved.pop('LAYR_PERSON_LOGIN')
 check(json.loads(run(['person','show','--site',site,'--json'],saved).stdout)['login']=='ann@example.com','saved login signs in on the next CLI invocation')
 before=stats['session'];faults['reject_session']=True
 run(['person','show','--site',site,'--json'],ann)
 check(stats['session']==before+2 and stats['renew']==1,'a rejected session renews once and repeats the authorized read')
 before=stats['session'];faults['short_session']=True
 run(['person','show','--site',site,'--json'],ann)
 check(stats['session']==before+2,'session nearing expiry renews before the next person call')
 before=stats['session'];faults['always_reject']=True
 p=run(['person','show','--site',site],ann,False);faults.pop('always_reject')
 check(p.returncode!=0 and stats['session']==before+2,'persistent unauthorized response stops after one renewal')
 before=stats['session'];faults['expired_challenge']=True
 p=run(['person','show','--site',site],ann,False)
 check(p.returncode!=0 and stats['session']==before,'expired challenge fails before signature exchange')
 bob=dict(base,LAYR_PERSON_DIR=str(root/'bob'),LAYR_PERSON_LOGIN='bob@example.com')
 run(['person','keys','--paper-key-out',str(root/'bob.paper')],bob)
 bob.update(LAYR_PERSON_KEY_FILE=str(root/'bob.paper'),LAYR_PERSON_LOGIN='ann@example.com',LAYR_GATEWAY_CA_FILE=str(root/'tls-ca.pem'))
 p=run(['person','show','--site',site],bob,False)
 check(p.returncode!=0 and '401' in p.stderr,'wrong signing key cannot sign in as another login')
 p=run(['person','keys','--site',site,'--invite','registration-invite'],ann,False)
 check(p.returncode!=0 and '401' in p.stderr and stats['registration']==1,'used invite cannot overwrite existing person keys')
 # Create a real project, sealed person invite and person slot, then accept over HTTPS.
 run(['init','session-project','-q'],base)
 project=dict(base,LAYR_PROJECT='session-project',LAYR_LINE='main')
 line=pathlib.Path(base['LAYR_ROOT'])/'projects/session-project/lines/main'
 (line/'hello.txt').write_text('session request test\n')
 run(['add','hello.txt'],project);run(['save','-qm','Session test'],project)
 run(['remote','add','archive','file://'+str(root/'archive'),'--paper-key-out',str(root/'store.paper')],project)
 run(['push','archive','-q'],project)
 actor=dict(ann,LAYR_PROJECT='session-project',LAYR_LINE='main')
 run(['remote','grant','archive','--person','ann@example.com','--recipient',people['ann@example.com']['recipient'],'--invite-out',str(root/'invite.age')],actor)
 people['ann@example.com']['inbox']=[{'id':'1-test','data':base64.b64encode((root/'invite.age').read_bytes()).decode()}]
 run(['person','accept','--site',site,'--invite','1-test'],ann)
 check(stats['index']==1 and stats['inbox']==1 and stats['slots']==1,'gateway invite acceptance authenticates person slot, index update and inbox deletion')
 run(['remote','add','web','layr+'+site+'/project'],actor)
 before=stats['requests'];faults['reject_request_session']=True
 request=json.loads(run(['request','approve','head','--person','--remote','web','--json'],actor).stdout)
 check(stats['requests']==before+1 and stats['request_attempts']==2 and request['request']['v']==2,'real person request uploads an encrypted v2 envelope with a session bearer')
 check(stats['registration']==1,'person request never re-registers or overwrites the person key')
 # Repeat through an actual project epoch, including the executor's claim write.
 run(['remote','rotate','archive','--reason','session integration'],project)
 epoch=json.loads(next((root/'archive/keys').glob('*.json')).read_text())['r']['data']['epoch']
 before=stats['request_attempts'];faults['reject_request_session']=True
 rotated=json.loads(run(['request','approve','head','--person','--remote','web','--json'],actor).stdout)
 check(stats['epoch_requests']==1 and stats['request_attempts']==before+2 and rotated['request']['epoch']==epoch,'epoch request uses a session bearer and renews once after rejection')
 check((root/'archive/project/epochs'/epoch/'requests'/f"{rotated['request']['id']}.json").is_file(),'session request stores its envelope only in the current epoch namespace')
 token=root/'writer.token';token.write_text('v1.write.signinTest');token.chmod(0o600)
 executor=dict(actor,LAYR_GATEWAY_TOKEN_FILE=str(token))
 run(['requests','apply','--remote','web','--json'],executor)
 check(stats['epoch_claims']==1 and (root/'archive/project/epochs'/epoch/'claims'/f"{rotated['request']['id']}.json").is_file(),'executor claims the epoch request with its machine write bearer')
 check(stats['epoch_proofs']>0,'epoch read proof exchanges retain the person session bearer')
 run(['requests','apply','--remote','web','--json'],executor)
 check(stats['epoch_claims']==1,'repeating the session-authenticated executor creates no duplicate epoch claim')
 fresh=dict(base,LAYR_PERSON_DIR=str(root/'fresh'),LAYR_PERSON_LOGIN='fresh@example.com',LAYR_GATEWAY_CA_FILE=str(root/'tls-ca.pem'))
 before=stats['session']
 run(['person','keys','--site',site,'--invite','fresh-invite','--paper-key-out',str(root/'fresh.paper')],fresh)
 check(stats['registration']==2 and stats['session']==before+1,'fresh invite registration signs in directly from generated in-memory keys')
 check(not errors,'stand-in observed only valid HTTP requests')
 # Token strings never reach captured command output or local person files.
 files=b''.join(p.read_bytes() for p in (root/'ann').iterdir() if p.is_file())
 check(all(t.encode() not in files for t in sessions),'session tokens are not persisted in person files')
 (root/'signin-counts').write_text(str(checks))
finally:
 server.shutdown();server.server_close();thread.join()
PY
PASS=$((PASS + $(cat "$T/signin-counts")))
finish
