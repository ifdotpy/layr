#!/bin/bash
# Rootless WebDAV of a real encrypted store. The paper key never reaches the test output.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/src"
l init disk --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/disk/lines/main"
printf abc > a.txt
printf xyz > b.txt
printf '<xml>&\n' > 'space ü.txt'
ln -s a.txt inside
ln -s /etc/shadow outside
l add -A
l commit -qm disk-start
l remote add origin "file://$T/remote" --paper-key-out "$T/paper" >/dev/null
l push origin >/dev/null
l branch empty -q
l branch -d empty -q
l branch other -q
echo saved > autosaved.txt
l save -q
l push origin --auto >/dev/null
chmod 755 "$T"
id layr-disk-test >/dev/null 2>&1 || useradd -m layr-disk-test
cp "$LAYR" "$T/layr"
chmod 755 "$T/layr"
mkdir "$T/home"
chown -R layr-disk-test "$T/home" "$T/remote" "$T/paper"
export DISK_TEST_T=$T DISK_TEST_STORE=$LAYR_ROOT
python3 - <<'CHECK'
import http.client,json,os,pathlib,re,signal,subprocess,time
root=pathlib.Path(os.environ['DISK_TEST_T'])
binary=str(root/'layr'); remote='file://'+str(root/'remote')
command=['runuser','-u','layr-disk-test','--','env','HOME='+str(root/'home'),
 'XDG_CACHE_HOME='+str(root/'home/cache'),'LAYR_ROOT='+str(root/'no-project-store'),
 'LAYR_SOCKET='+str(root/'no-service'),'LAYR_DIRECT=0',binary,'mount',remote,
 '--listen','127.0.0.1:0','--cache-size','5','--paper-key-file',str(root/'paper')]
log=open(root/'disk.log','w+')
process=subprocess.Popen(command,stdout=log,stderr=log,start_new_session=True)
checks=0
try:
 deadline=time.monotonic()+30; port=None
 while time.monotonic()<deadline:
  log.flush(); log.seek(0); text=log.read()
  match=re.search(r'http://127\.0\.0\.1:(\d+)/',text)
  if match: port=int(match[1]); break
  if process.poll() is not None: raise AssertionError('rootless disk stopped before startup')
  time.sleep(.1)
 assert port is not None,'rootless disk did not start'
 def request(method,path='/',headers=None):
  c=http.client.HTTPConnection('127.0.0.1',port,timeout=10)
  c.request(method,path,headers=headers or {})
  r=c.getresponse(); result=(r.status,dict(r.getheaders()),r.read()); c.close(); return result
 def check(condition,name):
  global checks
  assert condition,name
  checks+=1
 status,headers,body=request('OPTIONS'); check(status==200 and 'PUT' not in headers['Allow'],'read-only options')
 status,_,body=request('PROPFIND','/',{'Depth':'1'}); check(status==207 and b'/main/' in body and b'/other/' in body,'line folders')
 status,_,body=request('PROPFIND','/main/',{'Depth':'1'}); check(status==207 and b'space%20%C3%BC.txt' in body and b'&lt;' not in body,'encoded paths')
 check(request('PROPFIND','/',{'Depth':'infinity'})[0]==403,'bounded depth')
 check(request('PUT','/main/a.txt')[0]==405,'writes refused')
 check(request('GET','/main/%2e%2e/%2e%2e/etc/shadow')[0]==400,'traversal refused')
 check(request('GET','/main/a.txt',{'Host':'attacker.example'})[0]==403,'foreign host refused')
 check(request('GET','/main/a.txt',{'Origin':'https://attacker.example'})[0]==403,'browser origin refused')
 status,headers,body=request('GET','/main/a.txt'); check(status==200 and body==b'abc','file decrypt')
 check(request('HEAD','/main/a.txt')[1]['Content-Length']=='3','HEAD length')
 check(request('GET','/main/a.txt',{'Range':'bytes=1-2'})[2]==b'bc','closed range')
 check(request('GET','/main/a.txt',{'Range':'bytes=-1'})[2]==b'c','suffix range')
 check(request('HEAD','/main/a.txt',{'Range':'bytes=1-'})[0]==206,'HEAD range')
 check(request('GET','/main/a.txt',{'Range':'bytes=8-'})[0]==416,'invalid range')
 check(request('GET','/main/a.txt',{'If-None-Match':headers['ETag']})[0]==304,'conditional GET')
 check(request('GET','/main/inside')[2]==b'abc','internal link')
 check(request('GET','/main/outside')[2]==b'/etc/shadow','external link remains text')
 status,_,body=request('GET','/.layr/main.json'); meta=json.loads(body)
 check(status==200 and set(meta)=={'state','kind','machine','time'} and meta['kind']=='auto','separate state metadata')
 check(request('GET','/main/autosaved.txt')[2]==b'saved\n','newer automatic state')
 check(request('GET','/main/.layr-state')[0]==404,'source tree has no metadata file')
 cache=root/'home/cache/layr/mount'
 request('GET','/main/b.txt')
 check(sum(p.stat().st_size for p in cache.iterdir())<=5,'cache bound')
 import hashlib
 hashed=cache/hashlib.sha256(b'xyz').hexdigest(); hashed.write_bytes(b'bad')
 check(request('GET','/main/b.txt')[2]==b'xyz','corrupt cache is replaced')
 check((cache.stat().st_mode&0o777)==0o700 and all((p.stat().st_mode&0o777)==0o600 for p in cache.iterdir()),'cache permissions')
 check(not (root/'no-project-store').exists(),'no local project store')
 paper=(root/'paper').read_bytes().strip()
 log.flush(); log.seek(0)
 check(paper not in log.read().encode() and paper not in (root/'home/.config/layr/mount.json').read_bytes(),'key absent from output and preferences')
 # A writer publishes a new head while this process serves its old snapshot.
 writer=os.environ.copy(); writer['LAYR_ROOT']=os.environ['DISK_TEST_STORE']; writer.pop('LAYR_SOCKET',None)
 working=pathlib.Path(writer['LAYR_ROOT'])/'projects/disk/lines/main'
 (working/'a.txt').write_text('new')
 for args in [['add','a.txt'],['commit','-qm','disk-refresh'],['push','origin']]:
  subprocess.run([binary,*args],cwd=working,env=writer,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
 check(request('GET','/main/a.txt')[2]==b'abc','old view stays consistent until refresh')
 deadline=time.monotonic()+150
 while time.monotonic()<deadline:
  if request('GET','/main/a.txt')[2]==b'new': break
  time.sleep(1)
 else: raise AssertionError('the two-minute refresh did not publish the new view')
 check(True,'refresh publishes the new head')
 print(f'PASS rootless WebDAV ({checks} checks)')
finally:
 os.killpg(process.pid,signal.SIGTERM)
 process.wait(timeout=10)
 log.close()
CHECK
PASS=$((PASS + 1))
finish
