#!/bin/bash
# ci: macos overlayfs
# A warm fetch avoids state listings and repeats descriptor pin checks.
. "$(dirname "$0")/lib.sh"
skip_in_check_sandbox "it binds a local HTTPS server"
set -e
new_fs "$T/executor" 2G
export BOOTSTRAP_TEST_ROOT="$T" BOOTSTRAP_TEST_BINARY="$LAYR"
python3 - <<'PY'
import base64,http.server,json,os,pathlib,secrets,ssl,subprocess,threading,time,urllib.parse
root=pathlib.Path(os.environ['BOOTSTRAP_TEST_ROOT']);binary=os.environ['BOOTSTRAP_TEST_BINARY'];base=os.environ.copy()
for key in ['LAYR_PERSON_KEY_FILE','LAYR_PERSON_LOGIN','LAYR_PAPER_KEY_FILE','LAYR_GATEWAY_TOKEN_FILE','LAYR_PROJECT','LAYR_LINE']:
 base.pop(key,None)
checks=0;outputs=[]
def check(value,label):
 global checks
 assert value,label
 checks+=1;print('PASS',label,flush=True)
def run(args,env,success=True):
 cwd=pathlib.Path(env['LAYR_ROOT'])/'projects'/env['LAYR_PROJECT']/'lines/main' if env.get('LAYR_PROJECT') else root
 p=subprocess.run([binary,*args],env=env,cwd=cwd,text=True,capture_output=True,timeout=90)
 outputs.append(p.stdout+p.stderr)
 if success: assert p.returncode==0,(args,p.stderr,globals().get('requests',[])[-4:])
 return p
def openssl(args): subprocess.run(['openssl',*args],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
executor=dict(base,LAYR_ROOT=str(root/'executor/store'))
machine=json.loads(run(['remote','self','--json'],executor).stdout)
run(['init','project','-q'],base)
run(['--project','project','protect','main','--remove'],base)
owner=dict(base,LAYR_PROJECT='project',LAYR_LINE='main')
run(['remote','add','archive','file://'+str(root/'archive'),'--paper-key-out',str(root/'paper')],owner)
run(['remote','trust','archive',machine['id'],'--public-key',machine['public_key']],owner)
run(['push','archive','-q'],owner)
# Use an existing age-encrypted record as the stand-in's read challenge. The CLI must
# open it with its own machine slot and return plaintext, as for a gateway challenge.
challenge=base64.b64encode(next((root/'archive/records').rglob('*.age')).read_bytes()).decode()
openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(root/'ca-key.pem'),'-out',str(root/'ca.pem'),'-days','1','-subj','/CN=Bootstrap test CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign'])
openssl(['req','-new','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-key.pem'),'-out',str(root/'tls.csr'),'-subj','/CN=localhost'])
(root/'tls-ext.cnf').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n')
openssl(['x509','-req','-in',str(root/'tls.csr'),'-CA',str(root/'ca.pem'),'-CAkey',str(root/'ca-key.pem'),'-CAcreateserial','-out',str(root/'tls-cert.pem'),'-days','1','-extfile',str(root/'tls-ext.cnf')])
grant_p=secrets.token_hex(32);grant_q=secrets.token_hex(32);read_token='v1.read.fixture';requests=[];errors=[]
for name,value in [('grant-p',grant_p),('grant-q',grant_q)]:
 path=root/name;path.write_text(value);os.chmod(path,0o600)
own_slot='slots/machine-'+machine['id']+'.age'
def discovery(method,path):
 parsed=urllib.parse.urlsplit(path);query=urllib.parse.parse_qs(parsed.query)
 return method=='GET' and parsed.path=='/v1/p/project/objects' and query.get('prefix')==['keys/'] and set(query)<= {'prefix','limit','continuation'} and query.get('limit',['1000'])==['1000']
def object_page(wanted,query):
 assert set(query)<= {'prefix','limit','continuation','delimiter'} and query.get('limit',['1000'])==['1000']
 keys=sorted(str(p.relative_to(root/'archive')) for p in (root/'archive').rglob('*') if p.is_file())
 # A folder listing (delimiter=/): the folders one level below the prefix, as the gateway gives them.
 if 'delimiter' in query:
  assert query['delimiter']==['/'] and 'continuation' not in query
  return {'objects':[],'prefixes':sorted({wanted+k[len(wanted):].split('/')[0]+'/' for k in keys if k.startswith(wanted) and '/' in k[len(wanted):]})}
 files=sorted([{'key':str(p.relative_to(root/'archive')),'modified':int(p.stat().st_mtime*1000)} for p in (root/'archive').rglob('*') if p.is_file() and str(p.relative_to(root/'archive')).startswith(wanted)],key=lambda row:row['key'])
 if wanted=='states/':
  files.extend({'key':f'states/01a12290-0000-7000-8000-{n:012x}/deadbeef.0000.data.age','modified':0} for n in range(12000))
  files.sort(key=lambda row:row['key'])
 offset=int(query.get('continuation',['0'])[0]);assert offset>=0
 # One entry exercises continuation even on this small bootstrap fixture.
 page=files[offset:offset+1000]
 return {'objects':page,'continuation':str(offset+1000) if offset+1000<len(files) else None}
response_bytes=[]
class Gateway(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def reply(self,status,body):
  body=body if isinstance(body,bytes) else json.dumps(body).encode()
  response_bytes.append(len(body))
  self.send_response(status);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self): self.dispatch()
 def do_POST(self): self.dispatch()
 def do_PUT(self): self.dispatch()
 def dispatch(self):
  try: self.api()
  except Exception as error:
   errors.append(type(error).__name__);self.reply(500,{})
 def api(self):
  parsed=urllib.parse.urlsplit(self.path);path=parsed.path;auth=self.headers.get('Authorization')
  kind='P-grant' if auth=='Bearer '+grant_p else 'Q-grant' if auth=='Bearer '+grant_q else 'read' if auth=='Bearer '+read_token else 'none' if auth is None else 'unexpected'
  requests.append((self.command,self.path,kind))
  prefix='/v1/p/project/'
  if not path.startswith(prefix):
   assert kind not in ['P-grant','Q-grant']
   self.reply(404,{});return
  action=path[len(prefix):]
  if action=='slots':
   errors.append('aggregate slots requested');self.reply(500,{});return
  bootstrap=(self.command=='GET' and action in ['store.json',own_slot,'challenge']) or (self.command=='POST' and action=='token')
  if bootstrap:
   if kind!='P-grant': self.reply(404,{'error':'no such project'});return
   assert not parsed.query
   if action=='store.json': self.reply(200,(root/'archive/store.json').read_bytes());return
   if action==own_slot: self.reply(200,(root/'archive'/own_slot).read_bytes());return
   if action=='challenge': self.reply(200,{'challenge':challenge});return
   body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
   assert isinstance(body.get('challenge'),str) and '"machine"' in body['challenge']
   self.reply(200,{'token':read_token,'expires':int(time.time()*1000)+300000});return
  # Signed epoch discovery is visible before opening the current machine slot.
  # The grant permits only the exact discovery namespace, never encrypted data.
  if kind=='P-grant' and discovery(self.command,self.path):
   self.reply(200,object_page('keys/',urllib.parse.parse_qs(parsed.query)));return
  if self.command=='PUT' and action.startswith('objects/') and kind=='P-grant':
   name=action[len('objects/'):];assert '..' not in pathlib.PurePosixPath(name).parts
   assert name.startswith(('records/','states/','index/'))
   body=self.rfile.read(int(self.headers.get('Content-Length','0')));file=root/'archive'/name
   if file.exists(): self.reply(412,b'');return
   file.parent.mkdir(parents=True,exist_ok=True);file.write_bytes(body);self.reply(201,b'');return
  # Possession of the grant is not substituted for the normal encrypted read proof.
  assert kind not in ['P-grant','Q-grant']
  if kind!='read': self.reply(401,{});return
  if self.command=='GET' and action=='objects':
   wanted=urllib.parse.parse_qs(parsed.query).get('prefix',[''])[0]
   self.reply(200,object_page(wanted,urllib.parse.parse_qs(parsed.query)));return
  if self.command=='GET' and action.startswith('objects/'):
   name=action[len('objects/'):];assert '..' not in pathlib.PurePosixPath(name).parts
   file=root/'archive'/name
   self.reply(200,file.read_bytes()) if file.is_file() else self.reply(404,{})
   return
  if self.command=='PUT' and action=='objects/'+own_slot:
   self.rfile.read(int(self.headers.get('Content-Length','0')));self.reply(412,b'');return
  self.reply(404,{})
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Gateway)
context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(root/'tls-cert.pem',root/'tls-key.pem');server.socket=context.wrap_socket(server.socket,server_side=True)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
site='https://localhost:'+str(server.server_port);url='layr+'+site+'/project'
valid=dict(executor,LAYR_GATEWAY_CA_FILE=str(root/'ca.pem'),LAYR_GATEWAY_TOKEN_FILE=str(root/'grant-p'),LAYR_PAPER_KEY_FILE=str(root/'paper'))
try:
 from collections import Counter
 p=run(['clone',url,'copy','--mode','secret','--heads','--overlay','main','-q'],valid)
 check(any(urllib.parse.parse_qs(urllib.parse.urlsplit(path).query).get('prefix')==['states/'] for _,path,_ in requests),'a clone with a missing state still lists all attempts')
 check(any('/objects/states/' in path and path.endswith('.tree.age') for _,path,_ in requests),'the missing state is still downloaded and verified')
 sync=dict(valid,LAYR_PROJECT='copy',LAYR_TRACE='1')
 for number in range(2):
  requests.clear();response_bytes.clear()
  p=run(['fetch','origin','--heads','--overlay','main'],sync)
  print('NOOP',number+1,p.stdout.strip(),flush=True)
  print(p.stderr,flush=True)
  print('HTTP calls:',len(requests),'response body bytes:',sum(response_bytes),flush=True)
  listings=[urllib.parse.parse_qs(urllib.parse.urlsplit(path).query) for method,path,_ in requests if method=='GET' and urllib.parse.urlsplit(path).path=='/v1/p/project/objects']
  check('0 records, 0 states, 0 bytes' in p.stdout,'fetch is a no-op in a fresh process')
  check(not any(q.get('prefix')==['states/'] for q in listings),'a no-op fetch never lists states, including historical data pages')
  check(sum(path=='/v1/p/project/store.json' for _,path,_ in requests)>1,'a connected fetch rechecks the live descriptor')
  check(sum(q.get('prefix')==['overlays/'] and q.get('delimiter')==['/'] for q in listings)<=1,'overlay discovery lists folders at most once')
  check(len(requests)<=13,'a no-op fetch makes at most thirteen HTTP requests with live pin checks')
  for request,count in sorted(Counter((method,path) for method,path,_ in requests).items()):
   print(count,*request,flush=True)
 # A new process must still reject a descriptor that no longer matches its pinned project.
 descriptor=root/'archive/store.json';original=descriptor.read_bytes()
 changed=json.loads(original);changed['project']='01a12290-0000-7000-8000-ffffffffffff'
 descriptor.write_text(json.dumps(changed))
 try:
  p=run(['fetch','origin','--heads','--overlay','main'],sync,False)
  check(p.returncode!=0 and 'store project' in p.stderr,'a later fetch still verifies the descriptor pin')
 finally:
  descriptor.write_bytes(original)
 check(not errors,'counting gateway accepted the requests')
 (root/'bootstrap-counts').write_text(str(checks))
finally:
 server.shutdown();server.server_close();thread.join(timeout=10)
PY
PASS=$((PASS + $(cat "$T/bootstrap-counts")))
finish
