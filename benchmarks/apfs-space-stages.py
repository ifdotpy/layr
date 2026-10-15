import subprocess,pathlib,plistlib,os,json,time
root=pathlib.Path('/tmp/layr-apfs-space-20261016-v2').resolve(); rows=[]
def call(args,**kw):
 r=subprocess.run(args,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,**kw)
 if r.returncode:raise RuntimeError((args,r.returncode,r.stdout))
 return r.stdout
for name,binary in [('baseline','/tmp/layr-perf-baseline-76623f1'),('native','/tmp/layr-perf-candidate-v2')]:
 mount=root/(name+'-0')
 call(['hdiutil','attach','-nobrowse','-mountpoint',str(mount),str(root/(name+'-0.sparseimage'))])
 try:
  env=dict(os.environ,LAYR_ROOT=str(mount/'store'),LAYR_AUTHOR_NAME='Benchmark',LAYR_AUTHOR_EMAIL='bench@example.invalid')
  for k in ['LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE','LAYR_APFS_EXPERIMENT_CLONE_TREE']:env.pop(k,None)
  if name=='native':env['LAYR_APFS_EXPERIMENT_CLONE_TREE']='1'
  main=mount/'store/projects/space/lines/main'
  def run(cwd,*args):call([binary,*args],cwd=cwd,env=env)
  def measure(phase):
   call(['/bin/sync']);time.sleep(2)
   d=plistlib.loads(call(['diskutil','info','-plist',str(mount)]))
   row=dict(variant=name,phase=phase,used_bytes=d['APFSContainerSize']-d['APFSContainerFree']);rows.append(row);print(row,flush=True)
   (root/'stages.json').write_text(json.dumps(rows,indent=2))
  measure('start');run(main,'branch','-q','isolate');measure('branch')
  work=main.parent/'isolate'
  with (work/'large.bin').open('r+b') as f:f.write(b'Z'*4096)
  measure('write_4k');run(work,'add','large.bin');measure('add');run(work,'commit','-qm','isolate');measure('commit');run(main,'fsck')
 finally:call(['hdiutil','detach',str(mount)])
