#!/usr/bin/env python3
"""Paired CLI measurements, alternating binary order on the same APFS image."""
import argparse, hashlib, json, os, pathlib, plistlib, shutil, subprocess, time
p=argparse.ArgumentParser()
p.add_argument('--root',required=True)
p.add_argument('--baseline',required=True)
p.add_argument('--candidate',required=True)
p.add_argument('--cases',default='files,large')
a=p.parse_args()
root=pathlib.Path(a.root).resolve();root.mkdir(parents=True,exist_ok=False)
bins={k:str(pathlib.Path(getattr(a,k)).resolve()) for k in ('baseline','candidate')}
rows=[]
report={'binaries':{k:{'path':v,'sha256':hashlib.sha256(pathlib.Path(v).read_bytes()).hexdigest()} for k,v in bins.items()},'rows':rows,'host_free_bytes':shutil.disk_usage(root).free,'notes':['Warm/mixed cache; no cache purge.','Three trials; alternate binary order.','Container allocation includes metadata; sync excluded from timings.']}
def save():(root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
def call(args,**kw):
 r=subprocess.run(list(map(str,args)),capture_output=True,text=True,**kw)
 if r.returncode:raise RuntimeError((args,r.returncode,r.stdout,r.stderr))
 return r
for kind,n in [('files',10000),('files',100000),('large',1000)]:
 if kind not in a.cases.split(','):continue
 image=root/f'{kind}-{n}.sparseimage';mount=root/f'{kind}-{n}';mount.mkdir()
 call(['hdiutil','create','-size','8g','-type','SPARSE','-fs','Case-sensitive APFS','-nospotlight','-volname','layr-paired',image])
 call(['hdiutil','attach','-nobrowse','-mountpoint',mount,image])
 try:
  env=dict(os.environ,LAYR_ROOT=str(mount/'store'),LAYR_TRACE='1',LAYR_AUTHOR_NAME='Benchmark',LAYR_AUTHOR_EMAIL='bench@example.invalid')
  for key in ('LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE'):env.pop(key,None)
  def run(variant,cwd,args,op=None,trial=None):
   start=time.perf_counter();r=call([bins[variant],*args],cwd=cwd,env=env);dt=time.perf_counter()-start
   if op:
    row={'kind':kind,'files':n,'variant':variant,'trial':trial,'operation':op,'wall':dt,'load':os.getloadavg(),'trace':r.stderr}
    rows.append(row);save();print(json.dumps({k:v for k,v in row.items() if k!='trace'}),flush=True)
   return r.stdout
  def used():
   call(['/bin/sync']);d=plistlib.loads(subprocess.check_output(['diskutil','info','-plist',str(mount)]));return d['APFSContainerSize']-d['APFSContainerFree']
  src=mount/'source';src.mkdir()
  for i in range(n):
   f=src/f'd{i//100:05}'/f'f{i:07}.txt';f.parent.mkdir(exist_ok=True);f.write_bytes((f'{i:07} stable\n'.encode()*400)[:4096])
  if kind=='large':
   block=os.urandom(1024**2)
   with (src/'large.bin').open('wb') as f:
    for _ in range(1024):f.write(block)
  run('baseline',mount,['init','paired','--from',str(src),'-q'])
  main=mount/'store/projects/paired/lines/main'
  for trial in range(3):
   order=['baseline','candidate'] if trial%2==0 else ['candidate','baseline']
   for variant in order:
    name=f'{variant}-{trial}';run(variant,main,['branch','-q',name],'branch',trial);work=main.parent/name
    if kind=='files':
     assert not run(variant,work,['status','--porcelain'],'status_clean',trial).strip()
     path=work/'d00000/f0000000.txt'
    else:path=work/'large.bin'
    old=path.stat()
    with path.open('r+b') as f:f.seek(8192 if kind=='large' else 0);f.write(b'x'*4096)
    os.utime(path,ns=(old.st_atime_ns,old.st_mtime_ns))
    status=run(variant,work,['status','--porcelain'],'status_dirty',trial)
    assert len(status.splitlines())==1 and status.startswith(' M '),status
    if n!=100000:
     if kind=='files':run(variant,work,['diff','--stat'],'diff',trial)
     before=used();run(variant,work,['add','-A'],'add',trial);after=used()
     rows.append({'kind':kind,'files':n,'variant':variant,'trial':trial,'operation':'add_allocation','bytes':after-before});save()
     staged=run(variant,work,['status','--porcelain'],'status_staged',trial)
     assert staged.startswith('M  ') and len(staged.splitlines())==1,staged
     run(variant,work,['commit','-qm','benchmark'],'commit',trial)
     run(variant,work,['fsck'],'fsck',trial)
     assert not run(variant,work,['status','--porcelain']).strip()
     if kind=='large':
      with (main/'large.bin').open('rb') as f:f.seek(8192);assert f.read(4096)==block[8192:12288]
      with path.open('rb') as f:f.seek(8192);assert f.read(4096)==b'x'*4096
    run(variant,main,['branch','-q','-D',name])
  report.setdefault('verified',[]).append([kind,n]);save()
 finally:call(['hdiutil','detach',mount])
 image.unlink()
report['complete']=True;save()
