#!/usr/bin/env python3
"""Alternate exact CLI workloads on separate lines from one common APFS state."""
import argparse,hashlib,json,os,pathlib,resource,subprocess,time
p=argparse.ArgumentParser();p.add_argument('--before',required=True);p.add_argument('--after',required=True);p.add_argument('--store',required=True);p.add_argument('--output',required=True);p.add_argument('--repeats',type=int,default=2);a=p.parse_args()
store=pathlib.Path(a.store).resolve();main=store/'projects/p0/lines/main';rows=[]
variants=[('baseline',a.before,False),('metadata',a.after,False),('native_tree',a.after,True)]
report={'binaries':{k:hashlib.sha256(pathlib.Path(v).read_bytes()).hexdigest() for k,v,_ in variants},'rows':rows,'note':'Native tree mode is experimental; no claim of production compatibility.'}
def run(binary,env,cwd,args,trial,label,operation=None):
 start=time.perf_counter();before=resource.getrusage(resource.RUSAGE_CHILDREN)
 r=subprocess.run([binary,*args],cwd=cwd,env=env,capture_output=True,text=True)
 wall=time.perf_counter()-start;after=resource.getrusage(resource.RUSAGE_CHILDREN)
 if r.returncode: raise RuntimeError((args,r.returncode,r.stderr))
 if operation:
  row={'trial':trial,'variant':label,'operation':operation,'wall':wall,'cpu':after.ru_utime+after.ru_stime-before.ru_utime-before.ru_stime,'load':os.getloadavg(),'trace':r.stderr}
  rows.append(row);pathlib.Path(a.output).write_text(json.dumps(report,indent=2));print(json.dumps({k:v for k,v in row.items() if k!='trace'}),flush=True)
 return r.stdout
for trial in range(a.repeats):
 for label,binary,native in variants[trial%3:]+variants[:trial%3]:
  env=dict(os.environ,LAYR_ROOT=str(store),LAYR_TRACE='1')
  for key in ('LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_APFS_EXPERIMENT_CLONE_TREE'):env.pop(key,None)
  if native:env['LAYR_APFS_EXPERIMENT_CLONE_TREE']='1'
  name=f'perf-{os.getpid()}-{trial}-{label}'
  run(binary,env,main,['branch','-q',name],trial,label,'branch')
  work=main.parent/name
  assert not run(binary,env,work,['status','--porcelain'],trial,label,'status_clean').strip()
  for n in range(10):
   with (work/f'd{n//100:05}'/f'f{n:07}.txt').open('ab') as f:f.write(b'paired experiment\n')
  run(binary,env,work,['diff','--stat'],trial,label,'diff_changed')
  run(binary,env,work,['add','-A'],trial,label,'add')
  run(binary,env,work,['commit','-qm','paired experiment'],trial,label,'commit')
  run(binary,env,work,['fsck'],trial,label)
