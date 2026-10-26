"""Real CLI clone over TLS using a many-state immutable synthetic archive."""
import base64,collections,http.server,json,os,pathlib,ssl,subprocess,threading,time,urllib.parse
root=pathlib.Path(os.environ['CDN_CLONE_TEST_ROOT']);archive=root/'archive';binary=os.environ['CDN_CLONE_TEST_BINARY']
def openssl(*args):subprocess.run(['openssl',*args],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
openssl('req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-ca-key.pem'),'-out',str(root/'tls-ca.pem'),'-days','1','-subj','/CN=Clone test CA')
openssl('req','-new','-newkey','rsa:2048','-nodes','-keyout',str(root/'tls-key.pem'),'-out',str(root/'tls.csr'),'-subj','/CN=localhost')
(root/'tls-ext.cnf').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n')
openssl('x509','-req','-in',str(root/'tls.csr'),'-CA',str(root/'tls-ca.pem'),'-CAkey',str(root/'tls-ca-key.pem'),'-CAcreateserial','-out',str(root/'tls-cert.pem'),'-days','1','-extfile',str(root/'tls-ext.cnf'))
counts=collections.Counter();signed=collections.Counter();reads=collections.Counter();sizes=[];errors=[]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def answer(self,status,body,kind='application/json'):
  data=body if isinstance(body,bytes) else json.dumps(body).encode();self.send_response(status);self.send_header('Content-Type',kind);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
 def link(self,key):
  expires=int(time.time())+300
  return {'url':f'https://localhost:{self.server.server_port}/o/project/{key}?Expires={expires}&Signature=fixture&Key-Pair-Id=K123&Hash-Algorithm=SHA256','expires':expires,'bytes':(archive/key).stat().st_size}
 def do_GET(self):
  try:self.get()
  except Exception as error:errors.append(type(error).__name__);self.answer(500,{'error':'fixture failure'})
 def get(self):
  parsed=urllib.parse.urlsplit(self.path);path=urllib.parse.unquote(parsed.path);query=urllib.parse.parse_qs(parsed.query)
  if path.startswith('/p/'):return self.answer(404,{})
  prefix='/v1/p/project/'
  if path.startswith('/o/project/'):
   assert not self.headers.get('Authorization');key=path[len('/o/project/'):];reads[key]+=1
   return self.answer(200,(archive/key).read_bytes(),'application/octet-stream')
  assert path.startswith(prefix);path=path[len(prefix):]
  if path=='store.json':return self.answer(200,(archive/'store.json').read_bytes())
  if path=='slots':return self.answer(200,{'slots':[{'name':p.stem,'data':base64.b64encode(p.read_bytes()).decode()} for p in (archive/'slots').glob('*.age')]})
  if path.startswith('slots/'):
   target=archive/path
   return self.answer(200,target.read_bytes()) if target.is_file() else self.answer(404,{})
  if path=='challenge':return self.answer(200,{'challenge':base64.b64encode(next((archive/'records').rglob('*.age')).read_bytes()).decode()})
  if path=='objects':return self.answer(200,{'objects':[{'key':str(p.relative_to(archive)),'modified':int(p.stat().st_mtime)} for p in archive.rglob('*') if p.is_file() and str(p.relative_to(archive)).startswith(query.get('prefix',[''])[0])]})
  assert path.startswith('objects/');key=path[len('objects/'):];target=archive/key
  if not target.is_file():return self.answer(404,{})
  if self.headers.get('x-layr-cdn')=='1':counts['single_sign']+=1;return self.answer(200,self.link(key),'application/vnd.layr.cdn+json')
  if key.startswith(('states/','records/')):counts['direct']+=1;reads[key]+=1
  return self.answer(200,target.read_bytes(),'application/octet-stream')
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  if self.path=='/v1/p/project/token':assert body['challenge'];return self.answer(200,{'token':'fixture.read','expires':int(time.time()*1000)+600000})
  assert self.path=='/v1/p/project/cdn' and body['epoch']=='root' and 0<len(body['keys'])<=256
  counts['batch']+=1;sizes.append(len(body['keys']));signed.update(body['keys'])
  if any(key.startswith('states/') for key in body['keys']):counts['state_batch']+=1
  return self.answer(200,{'objects':[{'status':200,**self.link(key)} for key in body['keys']]})
 def do_PUT(self):self.rfile.read(int(self.headers.get('Content-Length','0')));self.answer(412,{})
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(root/'tls-cert.pem',root/'tls-key.pem');server.socket=context.wrap_socket(server.socket,server_side=True)
threading.Thread(target=server.serve_forever,daemon=True).start()
try:
 env=dict(os.environ,LAYR_GATEWAY_CA_FILE=str(root/'tls-ca.pem'))
 result=subprocess.run([binary,'clone',f'layr+https://localhost:{server.server_port}/project','copy','-q'],env=env,text=True,capture_output=True,timeout=180)
 assert result.returncode==0,(result.stderr,dict(counts),errors)
 clone=pathlib.Path(env['LAYR_ROOT'])/'projects/copy/lines/main'
 original=subprocess.check_output(['git','-C',str(root/'source'),'rev-list','main']).splitlines();restored=subprocess.check_output(['git','-C',str(clone),'rev-list','HEAD']).splitlines();assert sorted(original)==sorted(restored)
 assert not errors and len(original)==131
 print(json.dumps({'measured':True,'counts':dict(counts),'batchSizes':sizes,'duplicateCiphertextReads':sum(n-1 for n in reads.values()),'duplicateSignedKeys':sum(n-1 for n in signed.values())}),flush=True)
 assert counts['direct']==0 and counts['single_sign']==0,dict(counts)
 assert max(reads.values())==1,collections.Counter(reads.values())
 assert max(signed.values())==1,collections.Counter(signed.values())
 assert counts['state_batch']<=3 and max(sizes)==256,(dict(counts),sizes)
 print(json.dumps({'status':'PASS','commits':len(original),'batchCount':counts['batch'],'batchSizes':sizes,'stateBatchCount':counts['state_batch'],'directGetCount':counts['direct'],'singleSignCount':counts['single_sign'],'uniqueCiphertextKeys':len(reads),'duplicateCiphertextReads':sum(n-1 for n in reads.values()),'duplicateSignedKeys':sum(n-1 for n in signed.values())}),flush=True)
finally:server.shutdown();server.server_close()
