#!/usr/bin/env python3
"""Measure change density, hard-link fallback, and a 4 KiB change in a large file."""
import argparse,hashlib,json,os,pathlib,plistlib,resource,shutil,subprocess,time
p=argparse.ArgumentParser();p.add_argument('--root',required=True);p.add_argument('--binary',required=True);a=p.parse_args()
root=pathlib.Path(a.root).resolve();root.mkdir(parents=True,exist_ok=False);binary=str(pathlib.Path(a.binary).resolve());rows=[]
report={'binary_sha256':hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest(),'rows':rows}
def save():(root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
def call(args,**kw):
 r=subprocess.run(list(map(str,args)),capture_output=True,text=True,**kw)
 if r.returncode:raise RuntimeError((args,r.returncode,r.stdout,r.stderr))
 return r
for kind,param in [('density',0),('density',10),('density',1000),('density',10000),('hardlink',1),('large',64*1024**2),('large',1024**3)]:
 image=root/f'{kind}-{param}.sparseimage';mount=root/f'{kind}-{param}';mount.mkdir()
 call(['hdiutil','create','-size','8g','-type','SPARSE','-fs','Case-sensitive APFS','-nospotlight','-volname','layr-workload',image]);call(['hdiutil','attach','-nobrowse','-mountpoint',mount,image])
 try:
  env=dict(os.environ,LAYR_ROOT=str(mount/'store'),LAYR_AUTHOR_NAME='Benchmark',LAYR_AUTHOR_EMAIL='bench@example.invalid',LAYR_TRACE='1')
  for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE']:env.pop(key,None)
  def used():
   call(['/bin/sync']);d=plistlib.loads(subprocess.check_output(['diskutil','info','-plist',str(mount)]));return d['APFSContainerSize']-d['APFSContainerFree']
  def run(cwd,args,trial,measure=True):
   t=time.perf_counter();cpu=resource.getrusage(resource.RUSAGE_CHILDREN);r=call([binary,*args],cwd=cwd,env=env);dt=time.perf_counter()-t;after=resource.getrusage(resource.RUSAGE_CHILDREN)
   if measure:
    row={'kind':kind,'parameter':param,'trial':trial,'operation':args[0],'wall':dt,'cpu':after.ru_utime+after.ru_stime-cpu.ru_utime-cpu.ru_stime,'load':os.getloadavg(),'trace':r.stderr};rows.append(row);save();print(json.dumps({k:v for k,v in row.items() if k!='trace'}),flush=True)
   return r.stdout
  src=mount/'source';src.mkdir();n=10000 if kind!='large' else 1000
  for i in range(n):
   f=src/f'd{i//100:05}'/f'f{i:07}.txt';f.parent.mkdir(exist_ok=True);f.write_bytes((f'{i:07} stable\n'.encode()*400)[:4096])
  if kind=='hardlink':os.link(src/'d00000/f0000000.txt',src/'alias')
  if kind=='large':
   block=os.urandom(1024**2)
   with (src/'large.bin').open('wb') as f:
    for _ in range(param//len(block)):f.write(block)
  run(mount,['init','work','--from',str(src),'-q'],0,False);main=mount/'store/projects/work/lines/main'
  for trial in range(3):
   name=f'trial{trial}';run(main,['branch','-q',name],trial);work=main.parent/name
   if kind=='hardlink':assert (work/'alias').stat().st_ino==(work/'d00000/f0000000.txt').stat().st_ino
   if kind=='density':
    for i in range(param):
     with (work/f'd{i//100:05}/f{i:07}.txt').open('ab') as f:f.write(b'change\n')
    status=run(work,['status','--porcelain'],trial);assert len(status.splitlines())==param,(param,len(status.splitlines()))
    run(work,['diff','--stat'],trial)
    if not param:continue
   elif kind=='large':
    with (work/'large.bin').open('r+b') as f:f.seek(8192);f.write(b'x'*4096)
   else:continue
   before=used();run(work,['add','-A'],trial);after=used()
   rows.append({'kind':kind,'parameter':param,'trial':trial,'operation':'add_allocation','bytes':after-before});save()
   run(work,['commit','-qm','benchmark'],trial);run(work,['fsck'],trial)
   assert not run(work,['status','--porcelain'],trial,False).strip()
   if kind=='large':
    with (main/'large.bin').open('rb') as f:f.seek(8192);assert f.read(4096)==block[8192:12288]
    with (work/'large.bin').open('rb') as f:f.seek(8192);assert f.read(4096)==b'x'*4096
  report.setdefault('verified',[]).append([kind,param]);save()
 finally:call(['hdiutil','detach',mount])
 image.unlink()
report['complete']=True;save()
