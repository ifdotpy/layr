#!/usr/bin/env python3
"""Paired real add/commit commands, with alternating order and checked results."""
import argparse,hashlib,json,os,platform,shutil,subprocess,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--baseline',required=True);p.add_argument('--candidate',required=True);p.add_argument('--root',required=True);a=p.parse_args()
root=Path(a.root).resolve();root.mkdir(parents=True)
bins={k:str(Path(getattr(a,k)).resolve()) for k in ['baseline','candidate']}
report={'platform':platform.platform(),'commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'binaries':{k:{'path':v,'sha256':hashlib.sha256(Path(v).read_bytes()).hexdigest()} for k,v in bins.items()},'rows':[],'verified':[],'complete':False}
def save():(root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
def call(args,**kw):
 r=subprocess.run(list(map(str,args)),capture_output=True,text=True,**kw)
 assert r.returncode==0,(args,r.returncode,r.stdout,r.stderr)
 return r
def erase(path):call(['chflags','-R','nouchg',path]);shutil.rmtree(path)
image=root/'test.sparseimage';mount=root/'volume';mount.mkdir()
call(['hdiutil','create','-size','12g','-type','SPARSE','-fs','Case-sensitive APFS','-nospotlight','-volname','layr-index-bench',image]);call(['hdiutil','attach','-nobrowse','-mountpoint',mount,image])
try:
 for n,changed in [(10000,1),(100000,10)]:
  case=mount/f'files-{n}';src=case/'source';src.mkdir(parents=True)
  for i in range(n):
   f=src/f'd{i//100:05}'/f'f{i:07}';f.parent.mkdir(exist_ok=True);f.write_bytes((f'{i:07} stable\n'.encode()*400)[:4096])
  env=dict(os.environ,LAYR_ROOT=str(case/'store'),LAYR_TRACE='1',LAYR_AUTHOR_NAME='Benchmark',LAYR_AUTHOR_EMAIL='bench@example.invalid')
  for key in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE']:env.pop(key,None)
  def run(binary,cwd,*args):return call([bins[binary],*args],cwd=cwd,env=env)
  run('baseline',case,'init','bench','--from',src,'-q');main=case/'store/projects/bench/lines/main'
  paths=[f'd{i//100:05}/f{i:07}' for i in range(changed)]
  for trial in range(3):
   for variant in (['baseline','candidate'] if trial%2==0 else ['candidate','baseline']):
    name=f'{variant}-{trial}';run(variant,main,'branch','-q',name);work=main.parent/name
    def measure(op,*args):
     start=time.perf_counter();r=run(variant,work,*args);wall=time.perf_counter()-start
     row={'files':n,'changed':changed,'trial':trial,'variant':variant,'operation':op,'wall':wall,'load':os.getloadavg(),'trace':r.stderr}
     report['rows'].append(row);save();print(json.dumps({k:v for k,v in row.items() if k!='trace'}),flush=True);return r
    for phase in [1,2]:
     for rel in paths:
      f=work/rel;st=f.stat()
      with f.open('r+b') as out:out.write(bytes([64+phase])*4096)
      os.utime(f,ns=(st.st_atime_ns,st.st_mtime_ns))
     added=measure('add_first' if phase==1 else 'add_existing','add',*([paths[0]] if changed==1 else ['-A']))
     if variant=='candidate':
      assert 'APFS copy selected' in added.stderr,added.stderr
      assert added.stderr.count('APFS clone ')==(1 if phase==1 else 0),added.stderr
     status=run(variant,work,'status','--porcelain').stdout
     assert len(status.splitlines())==changed and all(line.startswith('M  ') for line in status.splitlines()),status
    committed=measure('commit','commit','-qm','measured')
    if variant=='candidate':assert 'APFS promote index' in committed.stderr and 'APFS clone ' not in committed.stderr,committed.stderr
    assert not run(variant,work,'status','--porcelain').stdout;run(variant,work,'fsck')
    for rel in paths:
     assert (work/rel).read_bytes()==b'B'*4096
     assert (main/rel).read_bytes()==(src/rel).read_bytes()
    report['verified'].append({'files':n,'changed':changed,'trial':trial,'variant':variant});save()
    run(variant,main,'branch','-q','-D',name)
  erase(case)
 report['complete']=True;save()
finally:
 save();call(['hdiutil','detach',mount])
image.unlink()
