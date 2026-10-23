#!/bin/bash
# Pin recipient and project before any store upload or decryption.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init pinned --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/pinned/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l remote add attacker "file://$T/attacker" --paper-key-out "$T/attacker-paper" >/dev/null
l push archive -q
export PIN_TEST_T=$T
python3 - <<'PY'
import hashlib,http.client,http.server,json,os,pathlib,re,ssl,subprocess,threading,time,uuid
root=pathlib.Path(os.environ['PIN_TEST_T']); binary=os.environ['LAYR']
good=json.loads((root/'archive/store.json').read_text())
bad=json.loads((root/'attacker/store.json').read_text())
checks=0

def check(value,label):
 global checks
 assert value,label
 checks+=1

def run(args,env=None,success=True):
 p=subprocess.run([binary,*args],env=env,capture_output=True,text=True,timeout=30)
 if success: assert p.returncode==0,(args,p.stderr)
 return p

def refused(args,expected,env=None):
 p=run(args,env,False)
 check(p.returncode!=0 and all(x in p.stderr for x in expected),(args,p.stderr))
 return p

def hashes():
 return {str(p.relative_to(root/'archive')):hashlib.sha256(p.read_bytes()).hexdigest()
  for p in (root/'archive').rglob('*') if p.is_file()}

pin=json.loads(run(['config','cloud.remote.archive.store']).stdout)
check(pin=={k:good[k] for k in ['project','recipient']},'remote add saves both pins')
run(['remote','add','alias','file://'+str(root/'archive')])
run(['clone','file://'+str(root/'archive'),'copy','-q'])
clone_pin=json.loads(run(['--project','copy','config','cloud.remote.origin.store']).stdout)
check(clone_pin==pin,'clone saves both pins')
# The writer has no readable slot. It still must reject the changed recipient.
for p in (root/'archive/slots').iterdir(): p.unlink()
(root/'archive/store.json').write_text(json.dumps(bad))
before=hashes()
refused(['push','archive','-q'],[good['recipient'],bad['recipient']])
refused(['--project','copy','fetch','origin','-q'],[good['recipient'],bad['recipient']])
check(hashes()==before,'no ciphertext is published to the replacement recipient')
refused(['remote','trust-store','archive','--recipient',good['recipient']],[good['recipient'],bad['recipient']])
other=dict(bad,project=str(uuid.uuid4()))
(root/'archive/store.json').write_text(json.dumps(other))
refused(['remote','trust-store','archive','--recipient',bad['recipient']],[good['project'],other['project']])
(root/'archive/store.json').write_text(json.dumps(bad))
run(['remote','trust-store','archive','--recipient',bad['recipient'],'--json'])
check(json.loads(run(['config','cloud.remote.archive.store']).stdout)['recipient']==bad['recipient'],'explicit trust changes the signed pin')
run(['push','archive','-q'])
refused(['push','alias','-q'],[good['recipient'],bad['recipient']])
run(['remote','trust-store','alias','--recipient',bad['recipient']])
run(['push','alias','-q'])
check(hashes()!=before,'explicit trust permits write-only upload')
records=[json.loads(x)['r'] for x in run(['op','export']).stdout.splitlines()]
check(any(r['op']=='config' and r['data'].get('config',{}).get('cloud.remote.archive.store',{}).get('recipient')==bad['recipient']
 for r in records),'trust-store uses a signed config record')
# A fake HTTPS gateway changes its answer after configure, before the operation reads it.
subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-keyout',str(root/'tls.key'),
 '-out',str(root/'tls.crt'),'-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost,IP:127.0.0.1','-addext','basicConstraints=critical,CA:FALSE'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
class Gateway(http.server.BaseHTTPRequestHandler):
 count=0; swap=100; uploads=[]; other=[]
 def log_message(self,*args): pass
 def do_GET(self):
  if self.path.endswith('/store.json'):
   Gateway.count+=1
   body=json.dumps(bad if Gateway.count>=Gateway.swap else good).encode()
  else:
   Gateway.other.append(self.path); body=b'{"objects":[]}'
  self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_PUT(self):
  Gateway.uploads.append(self.rfile.read(int(self.headers.get('Content-Length','0'))))
  self.send_response(201);self.send_header('Content-Length','0');self.end_headers()
 def do_POST(self):
  Gateway.other.append(self.path);self.send_response(403);self.send_header('Content-Length','0');self.end_headers()
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Gateway)
ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);ctx.load_cert_chain(root/'tls.crt',root/'tls.key')
server.socket=ctx.wrap_socket(server.socket,server_side=True)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
env=os.environ.copy();env['LAYR_GATEWAY_CA_FILE']=str(root/'tls.crt')
url=f'layr+https://localhost:{server.server_port}/project'
try:
 run(['remote','add','gateway',url],env)
 for command,swap in [(['push','gateway','-q'],4),(['fetch','gateway','-q'],3),(['clone',url,'gateway-copy','-q'],4)]:
  Gateway.count=0;Gateway.swap=swap;Gateway.uploads=[];Gateway.other=[]
  refused(command,[good['recipient'],bad['recipient']],env)
  check(not Gateway.uploads and not Gateway.other,'changed gateway answer causes no upload, slot request or read proof')
 Gateway.count=0;Gateway.swap=1
 refused(['mount','gateway','--listen','127.0.0.1:0'],[good['recipient'],bad['recipient']],env)
 check(not Gateway.other,'named mount checks its signed pin before the slot')
finally:
 server.shutdown();server.server_close();thread.join()
# First-use URL mounts persist a separate pin, then reject project and recipient changes.
run(['push','attacker','-q'])
env=os.environ.copy();env['HOME']=str(root/'home');env['XDG_CONFIG_HOME']=str(root/'home/config')
url='file://'+str(root/'attacker')
command=['mount',url,'--paper-key-file',str(root/'attacker-paper'),'--listen','127.0.0.1:0']
log=open(root/'mount.log','w+')
process=subprocess.Popen([binary,*command],env=env,stdout=log,stderr=log)
try:
 deadline=time.monotonic()+20
 while time.monotonic()<deadline:
  log.seek(0);text=log.read()
  if re.search(r'http://127\.0\.0\.1:\d+/',text): break
  assert process.poll() is None,text
  time.sleep(.05)
 else: raise AssertionError('mount did not start')
 check(f"pinned store recipient {bad['recipient']} project {bad['project']}" in text,'first mount prints both pins')
 port=int(re.search(r'http://127\.0\.0\.1:(\d+)/',text)[1])
 (root/'attacker/store.json').write_text(json.dumps(dict(bad,recipient=good['recipient'])))
 connection=http.client.HTTPConnection('127.0.0.1',port,timeout=10)
 connection.request('GET','/main/notes.txt');response=connection.getresponse();response.read();connection.close()
 check(response.status==503,'mounted file read checks the pin before decryption')
 (root/'attacker/store.json').write_text(json.dumps(bad))
finally:
 process.terminate();process.wait(timeout=10);log.close()
pins=root/'home/config/layr/mount-pins.json'
check((pins.stat().st_mode&0o777)==0o600,'mount pins have mode 0600')
check(json.loads(pins.read_text())[url]=={k:bad[k] for k in ['project','recipient']},'URL pins persist')
(root/'attacker/store.json').write_text(json.dumps(dict(bad,recipient=good['recipient'])))
refused(command,[bad['recipient'],good['recipient']],env)
(root/'attacker/store.json').write_text(json.dumps(dict(bad,project=other['project'])))
refused(command,[bad['project'],other['project']],env)
run(['mount','--forget-pin',url],env)
check(url not in json.loads(pins.read_text()),'forget-pin removes only the URL pin')
print(f'PASS store pins ({checks} checks)')
PY
PASS=$((PASS + 1))
finish
