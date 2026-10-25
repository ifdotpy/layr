#!/bin/bash
# A hidden gateway project opens only through its scoped grant and the machine's own slot.
. "$(dirname "$0")/lib.sh"
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
 if success: assert p.returncode==0,(args,p.stderr)
 return p
def openssl(args): subprocess.run(['openssl',*args],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
executor=dict(base,LAYR_ROOT=str(root/'executor/store'))
machine=json.loads(run(['remote','self','--json'],executor).stdout)
run(['init','project','-q'],base)
owner=dict(base,LAYR_PROJECT='project',LAYR_LINE='main')
line=pathlib.Path(base['LAYR_ROOT'])/'projects/project/lines/main';(line/'hello.txt').write_text('private machine bootstrap\n')
run(['add','hello.txt'],owner);run(['save','-qm','Private gateway project'],owner)
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
class Gateway(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def reply(self,status,body):
  body=body if isinstance(body,bytes) else json.dumps(body).encode()
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
  # Possession of the grant is not substituted for the normal encrypted read proof.
  assert kind not in ['P-grant','Q-grant']
  if kind!='read': self.reply(401,{});return
  if self.command=='GET' and action=='objects':
   wanted=urllib.parse.parse_qs(parsed.query).get('prefix',[''])[0]
   self.reply(200,{'objects':[{'key':str(p.relative_to(root/'archive')),'modified':int(p.stat().st_mtime*1000)} for p in (root/'archive').rglob('*') if p.is_file() and str(p.relative_to(root/'archive')).startswith(wanted)]});return
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
valid=dict(executor,LAYR_GATEWAY_CA_FILE=str(root/'ca.pem'),LAYR_GATEWAY_TOKEN_FILE=str(root/'grant-p'))
try:
 p=run(['clone',url,'copy','-q'],valid)
 restored=pathlib.Path(valid['LAYR_ROOT'])/'projects/copy/lines/main/hello.txt'
 check(restored.read_text()=='private machine bootstrap\n','machine with the project grant clones and restores a hidden project')
 check(any(path=='/v1/p/project/'+own_slot and kind=='P-grant' for _,path,kind in requests),'machine fetches only its own slot directly with the scoped grant')
 check(any(method=='POST' and path=='/v1/p/project/token' and kind=='P-grant' for method,path,kind in requests),'the proof exchange token route carries the project grant')
 check(any(path=='/v1/p/project/challenge' and kind=='P-grant' for _,path,kind in requests),'the challenge route carries the project grant')
 check(any(path.startswith('/v1/p/project/objects?') and kind=='read' for _,path,kind in requests),'normal object reads use the read proof after bootstrap')
 sync=dict(valid,LAYR_PROJECT='copy');run(['fetch','origin','-q'],sync)
 check(restored.read_text()=='private machine bootstrap\n','the same machine synchronizes the hidden project with its grant')
 wrong=dict(valid,LAYR_GATEWAY_TOKEN_FILE=str(root/'grant-q'))
 p=run(['clone',url,'wrong','-q'],wrong,False)
 check(p.returncode!=0 and '404' in p.stderr and 'missing or wrong' in p.stderr,'a grant for another project is refused with a plain grant hint')
 missing=dict(valid);missing.pop('LAYR_GATEWAY_TOKEN_FILE')
 p=run(['clone',url,'missing','-q'],missing,False)
 check(p.returncode!=0 and '404' in p.stderr and 'missing or wrong' in p.stderr,'a missing grant is refused with a plain grant hint')
 check(not any(path=='/v1/p/project/slots' for _,path,_ in requests),'machine bootstrap never calls the aggregate slots route')
 allowed={'/v1/p/project/store.json','/v1/p/project/'+own_slot,'/v1/p/project/challenge','/v1/p/project/token'}
 check(all(path in allowed for _,path,kind in requests if kind=='P-grant'),'the project grant is attached only to that project bootstrap routes')
 check(not errors,'stand-in accepted only permitted routes and credentials')
 project=pathlib.Path(valid['LAYR_ROOT'])/'projects/copy'
 content=''.join(outputs).encode()+b''.join(p.read_bytes() for p in project.rglob('*') if p.is_file() and not p.is_symlink())
 check(all(token.encode() not in content for token in [grant_p,grant_q]),'grants never appear in command output, records, configuration or restored files')
 (root/'bootstrap-counts').write_text(str(checks))
finally:
 server.shutdown();server.server_close();thread.join(timeout=10)
PY
PASS=$((PASS + $(cat "$T/bootstrap-counts")))
finish
