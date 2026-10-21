#!/usr/bin/env python3
"""APFS measurements on an owned disk image. Does not modify product code."""
import argparse, hashlib, json, os, pathlib, platform, plistlib, shutil, subprocess, time
p=argparse.ArgumentParser();p.add_argument('--root',required=True);p.add_argument('--binary',required=True);p.add_argument('--helper',required=True);a=p.parse_args()
root=pathlib.Path(a.root).resolve();root.mkdir(exist_ok=False,parents=True)
repo=pathlib.Path(__file__).resolve().parents[2];binary=str(pathlib.Path(a.binary).resolve());helper=str(pathlib.Path(a.helper).resolve())
mount=root/'volume';mount.mkdir();image=root/'benchmark.sparseimage';rows=[]
report={'platform':platform.platform(),'commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip(),'binary_sha256':hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest(),'rows':rows,'notes':['Warm/mixed cache; no cache purge.','Timing excludes sync unless specified.','Image on host Data volume; host free space and load recorded.','Primitive tests do not provide complete Layr semantics.'],'host_free_bytes':shutil.disk_usage(root).free}
def save(): (root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
def call(args,**kw):
 r=subprocess.run([str(x) for x in args],capture_output=True,text=True,**kw)
 if r.returncode: raise RuntimeError((args,r.returncode,r.stdout,r.stderr))
 return r.stdout

def record(row):
 row['load']=os.getloadavg();rows.append(row);save();print(json.dumps(row),flush=True)
def primitive(op,path,trial,n,*extra):
 out=call([helper,op,path,*extra]);r=json.loads(out.splitlines()[-1]);r.update(files=n,trial=trial);record(r);return r

def allocation():
 call(['/bin/sync']);d=plistlib.loads(subprocess.check_output(['diskutil','info','-plist',str(mount)]));return d['APFSContainerSize']-d['APFSContainerFree']
def fixture(path,n):
 path.mkdir()
 for i in range(n):
  f=path/f'd{i//100:05}'/f'f{i:07}.txt';f.parent.mkdir(exist_ok=True);f.write_bytes((f'{i:07} data\n'.encode()*400)[:4096])
def digest(path):
 h=hashlib.sha256()
 with path.open('rb') as f:
  for b in iter(lambda:f.read(4*1024*1024),b''):h.update(b)
 return h.hexdigest()
call(['hdiutil','create','-size','8g','-type','SPARSE','-fs','Case-sensitive APFS','-nospotlight','-volname','layr-benchmark',image])
call(['hdiutil','attach','-nobrowse','-mountpoint',mount,image])
try:
 report['volume']=plistlib.loads(subprocess.check_output(['diskutil','info','-plist',str(mount)]));save()
 for n in [1000,10000,100000]:
  src=mount/f'primitive-{n}';fixture(src,n)
  expected=n+(n+99)//100
  for trial in range(3):
   for op in (['stat','bulk'] if trial%2==0 else ['bulk','stat']):
    r=primitive(op,src,trial,n);assert r['entries']==expected,(r,expected)
   dst=mount/'clone';primitive('clone',src,trial,n,dst)
   assert sum(len(fs) for _,_,fs in os.walk(dst))==n
   for idx in [0,n//2,n-1]:
    rel=f'd{idx//100:05}/f{idx:07}.txt';assert (src/rel).read_bytes()==(dst/rel).read_bytes()
   for op in ['unseal','xattr','seal','unseal']:primitive(op,dst,trial,n)
   t=time.perf_counter();shutil.rmtree(dst);record({'operation':'delete_tree','wall':time.perf_counter()-t,'files':n,'trial':trial})
  shutil.rmtree(src)
 # Actual extent-sharing measurements include sync and APFS metadata, outside timers.
 for size in [64*1024**2,1024**3]:
  src=mount/'large';block=os.urandom(1024**2)
  with src.open('wb') as f:
   for _ in range(size//len(block)):f.write(block)
  for trial in range(3):
   # A source clone retains the old version; modify only 4 KiB.
   old=mount/'old';call([helper,'clone',src,old])
   with src.open('r+b') as f:f.seek(8192);f.write(bytes([trial+65])*4096)
   expected_hash=digest(src)
   methods=['byte_copy','clone_replace'] if trial%2==0 else ['clone_replace','byte_copy']
   for method in methods:
    dst=mount/'destination';call([helper,'clone',old,dst]);before=allocation();start=time.perf_counter()
    if method=='byte_copy':
     with src.open('rb') as s,dst.open('wb') as d:shutil.copyfileobj(s,d,1024**2)
    else:
     tmp=mount/'new';call([helper,'clone',src,tmp]);os.replace(tmp,dst)
    wall=time.perf_counter()-start;after=allocation();assert digest(dst)==expected_hash
    record({'operation':method,'size':size,'trial':trial,'wall':wall,'allocated_delta':after-before})
    dst.unlink();allocation()
   old.unlink();allocation()
  src.unlink();allocation()
 sparse=mount/'sparse';
 with sparse.open('wb') as f:f.write(b'head');f.seek(1024**3-4);f.write(b'tail')
 for method in ['byte_copy','clone']:
  dst=mount/'sparse-copy';before=allocation();t=time.perf_counter()
  if method=='clone':call([helper,'clone',sparse,dst])
  else:
   with sparse.open('rb') as s,dst.open('wb') as d:shutil.copyfileobj(s,d,1024**2)
  elapsed=time.perf_counter()-t;after=allocation();assert digest(sparse)==digest(dst)
  record({'operation':'sparse_'+method,'wall':elapsed,'allocated_delta':after-before,'logical_size':dst.stat().st_size,'reported_blocks':dst.stat().st_blocks});dst.unlink();allocation()
 sparse.unlink()
 for trial in range(3):
  eventdir=mount/f'events-{trial}';eventdir.mkdir()
  for line in call([helper,'events',eventdir]).splitlines():
   row=json.loads(line);row['trial']=trial;record(row)
 for line in call([helper,'snapshot',mount]).splitlines():record(json.loads(line))
 for n,repeats in [(1000,3),(10000,3),(100000,1)]:
  target=mount/f'cli-{n}'
  out=call(['python3',repo/'benchmarks/backends.py','--binary',binary,'--root',target,'--files',n,'--repeats',repeats,'--trace'])
  shutil.copy2(target/'results.json',root/f'cli-{n}.json');record({'operation':'cli_complete','files':n,'repeats':repeats,'summary':out})
 report['complete']=True;save()
finally:
 save();call(['hdiutil','detach',mount])
