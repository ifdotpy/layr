#!/usr/bin/env python3
"""Compare Layr filesystem allocation on owned APFS or btrfs images."""
import argparse,hashlib,json,os,platform,plistlib,shutil,statistics,subprocess,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--backend',choices=['apfs','btrfs'],required=True);p.add_argument('--binary',required=True);p.add_argument('--root',required=True);p.add_argument('--trials',type=int,default=2);p.add_argument('--branches',type=int,default=16);a=p.parse_args()
root=Path(a.root).resolve();root.mkdir(parents=True,exist_ok=False)
binary=str(Path(a.binary).resolve());report={'backend':a.backend,'platform':platform.platform(),'binary_sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest(),'branches':a.branches,'cases':[],'complete':False}
def call(args,**kw):
 r=subprocess.run(list(map(str,args)),capture_output=True,text=True,**kw)
 if r.returncode:raise RuntimeError((args,r.returncode,r.stdout,r.stderr))
 return r.stdout
def save():(root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
def used(mount):
 samples=[];raw=[]
 for _ in range(3):
  call(['sync'])
  if a.backend=='btrfs':
   call(['btrfs','filesystem','sync',mount]);s=call(['btrfs','filesystem','usage','-b',mount]);raw.append(s)
   samples.append(int(next(x.split(':',1)[1].strip() for x in s.splitlines() if x.strip().startswith('Used:'))))
  else:
   r=subprocess.run(['diskutil','info','-plist',str(mount)],capture_output=True,check=True);info=plistlib.loads(r.stdout)
   samples.append(info['APFSContainerSize']-info['APFSContainerFree'])
  time.sleep(1)
 return {'used_bytes':statistics.median(samples),'samples':samples,'raw':raw}
def record(row,phase,count,mount):
 s={'phase':phase,'branches':count,**used(mount)};s['above_empty_bytes']=s['used_bytes']-row['empty']['used_bytes'];row['samples'].append(s);save()
 print(json.dumps({'case':row['case'],'trial':row['trial'],'backend':a.backend,**{k:v for k,v in s.items() if k!='raw'}}),flush=True)
if a.backend=='btrfs':report['btrfs_version']=call(['btrfs','version']);report['compression']='none; mkfs default metadata profile'
for case,n,size,large in [('empty-files',10000,0,0),('small-files',10000,4096,0),('large-file',1000,4096,1024**3)]:
 src=root/'source';src.mkdir();digest=hashlib.sha256()
 for i in range(n):
  rel=f'd{i//100:05}/f{i:07}';data=hashlib.shake_256(str(i).encode()).digest(size);f=src/rel;f.parent.mkdir(exist_ok=True);f.write_bytes(data);digest.update(rel.encode()+b'\0'+data)
 large_hash=hashlib.sha256()
 if large:
  with (src/'large.bin').open('wb') as f:
   for i in range(large//(1024**2)):
    data=hashlib.shake_256(f'large-{i}'.encode()).digest(1024**2);f.write(data);large_hash.update(data)
 for trial in range(a.trials):
  name=f'{case}-{trial}';image=root/(name+('.sparseimage' if a.backend=='apfs' else '.img'));mount=root/name;mount.mkdir()
  if a.backend=='apfs':
   call(['hdiutil','create','-size','8g','-type','SPARSE','-fs','Case-sensitive APFS','-nospotlight','-volname','LayrSpace',image]);call(['hdiutil','attach','-nobrowse','-mountpoint',mount,image])
  else:
   with image.open('xb') as f:f.truncate(8*1024**3)
   call(['mkfs.btrfs','-q','-f',image]);call(['mount','-o','loop',image,mount])
  row={'case':case,'trial':trial,'files':n,'file_bytes':size,'large_bytes':large,'source_sha256':digest.hexdigest(),'large_sha256':large_hash.hexdigest() if large else None,'samples':[],'verified':False};report['cases'].append(row)
  try:
   row['empty']=used(mount);save()
   env=dict(os.environ,LAYR_ROOT=str(mount/'store'),LAYR_AUTHOR_NAME='Benchmark',LAYR_AUTHOR_EMAIL='bench@example.invalid')
   for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_TEST_DIRECT_MAIN']:env.pop(key,None)
   def run(cwd,*args):return call([binary,*args],cwd=cwd,env=env)
   run(root,'init','space','--from',src,'-q');main=mount/'store/projects/space/lines/main';record(row,'main',0,mount)
   for i in range(1,a.branches+1):
    run(main,'line', 'new','-q',f'b{i}')
    if i in {1,4,8,a.branches}:record(row,'branches',i,mount)
   for i in range(1,a.branches+1):
    work=main.parent/f'b{i}'
    for k in range(10):
     f=work/f'd00000/f{k:07}'
     with f.open('r+b') as out:out.write(bytes([i])*4096 if size else bytes([i]))
    if large:
     with (work/'large.bin').open('r+b') as out:out.seek(i*1024**2);out.write(bytes([i])*4096)
    run(work,'add','-A');run(work,'save','-qm',f'edit {i}')
    assert not run(work,'status','--porcelain').strip()
    for k in range(10):assert (work/f'd00000/f{k:07}').read_bytes()==(bytes([i])*4096 if size else bytes([i]))
    if large:
     with (work/'large.bin').open('rb') as inp:inp.seek(i*1024**2);assert inp.read(4096)==bytes([i])*4096
    if i in {1,4,8,a.branches}:record(row,'commits',i,mount)
   run(main,'fsck')
   for k in range(n):
    rel=f'd{k//100:05}/f{k:07}';assert (main/rel).read_bytes()==(src/rel).read_bytes()
   if large:
    h=hashlib.sha256()
    with (main/'large.bin').open('rb') as inp:
     while True:
      data=inp.read(1024**2)
      if not data:break
      h.update(data)
    assert h.hexdigest()==large_hash.hexdigest()
   row['verified']=True;save()
  finally:
   call(['hdiutil','detach',mount] if a.backend=='apfs' else ['umount',mount])
  image.unlink();mount.rmdir()
 shutil.rmtree(src)
report['complete']=True;save()
