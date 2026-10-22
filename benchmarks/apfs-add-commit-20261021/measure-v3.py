#!/usr/bin/env python3
"""Measure real commands and isolated storage alternatives. Not a backend prototype."""
import argparse, ctypes, hashlib, json, os, platform, shutil, struct, subprocess, time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',required=True);p.add_argument('--root',required=True);p.add_argument('--smoke',action='store_true');a=p.parse_args()
root=Path(a.root).resolve();root.mkdir(parents=True,exist_ok=False)
binary=str(Path(a.binary).resolve());rows=[]
report={'commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'binary_sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest(),'platform':platform.platform(),'host_free_bytes':shutil.disk_usage(root).free,'rows':rows}
def save():(root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
def call(args,**kw):
 r=subprocess.run(list(map(str,args)),capture_output=True,text=True,**kw)
 if r.returncode:raise RuntimeError((args,r.returncode,r.stdout,r.stderr))
 return r
lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib',use_errno=True)
lib.clonefile.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_int];lib.clonefile.restype=ctypes.c_int
lib.chflags.argtypes=[ctypes.c_char_p,ctypes.c_uint];lib.chflags.restype=ctypes.c_int
lib.getattrlist.argtypes=[ctypes.c_char_p,ctypes.c_void_p,ctypes.c_void_p,ctypes.c_size_t,ctypes.c_ulong];lib.getattrlist.restype=ctypes.c_int
def checked(rc):
 if rc:raise OSError(ctypes.get_errno(),os.strerror(ctypes.get_errno()))
def clone(src,dst):checked(lib.clonefile(os.fsencode(src),os.fsencode(dst),1))
def flags(path,value):checked(lib.chflags(os.fsencode(path),value))
def identity(path):
 # ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_GEN_COUNT; FSOPT_NOFOLLOW | PACK_INVAL_ATTRS | ATTR_CMN_EXTENDED.
 attrs=ctypes.create_string_buffer(struct.pack('=HHIIIII',5,0,0x80080000,0,0,0,0))
 buf=ctypes.create_string_buffer(28)
 checked(lib.getattrlist(os.fsencode(path),attrs,buf,28,0x1|0x8|0x20))
 length,common=struct.unpack_from('=II',buf.raw);generation=struct.unpack_from('=I',buf.raw,24)[0]
 assert length==28 and common&0x80000 and generation
 s=path.stat();return (s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns,s.st_ctime_ns,generation)
def seal(root,seal_root=True):
 for base,dirs,files in os.walk(root,topdown=False):
  for f in files:flags(Path(base)/f,2)
  if seal_root or Path(base)!=root:flags(Path(base),2)
def erase(path):
 if path.exists():call(['chflags','-R','nouchg',path]);shutil.rmtree(path)
def measure(op,fn):
 t=time.perf_counter();out=fn();dt=time.perf_counter()-t
 row={'files':n,'changed':changed,'trial':trial,'operation':op,'wall':dt,'load':os.getloadavg()}
 if isinstance(out,subprocess.CompletedProcess):row['trace']=out.stderr
 rows.append(row);save();print(json.dumps({k:v for k,v in row.items() if k!='trace'}),flush=True)
 return out
image=root/'test.sparseimage';mount=root/'volume';mount.mkdir()
call(['hdiutil','create','-size','12g','-type','SPARSE','-fs','Case-sensitive APFS','-nospotlight','-volname','layr-add-commit',image])
call(['hdiutil','attach','-nobrowse','-mountpoint',mount,image])
try:
 for n,counts in ([(10,[1,10])] if a.smoke else [(10000,[1,100,10000]),(100000,[10])]):
  case=mount/f'files-{n}';case.mkdir();src=case/'source';src.mkdir()
  for i in range(n):
   f=src/f'd{i//100:05}'/f'f{i:07}.txt';f.parent.mkdir(exist_ok=True);f.write_bytes((f'{i:07} stable\n'.encode()*400)[:4096])
  env=dict(os.environ,LAYR_ROOT=str(case/'store'),LAYR_TRACE='1',LAYR_AUTHOR_NAME='Benchmark',LAYR_AUTHOR_EMAIL='bench@example.invalid')
  for key in ('LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE'):env.pop(key,None)
  def run(cwd,*args):return call([binary,*args],cwd=cwd,env=env)
  run(case,'init','bench','--from',src,'-q');main=case/'store/projects/bench/lines/main'
  for changed in counts:
   paths=[f'd{i//100:05}/f{i:07}.txt' for i in range(changed)]
   for trial in range(3):
    name=f'c{changed}-t{trial}';run(main,'branch','-q',name);work=main.parent/name
    for phase in [1,2]:
     for rel in paths:
      f=work/rel;old=f.stat()
      with f.open('r+b') as out:out.write(bytes([64+phase])*4096)
      os.utime(f,ns=(old.st_atime_ns,old.st_mtime_ns))
     st=measure(f'status_phase{phase}',lambda:run(work,'status','--porcelain'))
     assert len(st.stdout.splitlines())==changed
     args=['add',paths[0]] if changed==1 else ['add','-A']
     measure('add_first' if phase==1 else 'add_existing_index',lambda:run(work,*args))
     staged=run(work,'status','--porcelain').stdout
     assert len(staged.splitlines())==changed and all(x.startswith('M  ') for x in staged.splitlines())
    measure('commit',lambda:run(work,'commit','-qm','measured'))
    assert not run(work,'status','--porcelain').stdout.strip();run(work,'fsck')
    for rel in paths:
     assert (work/rel).read_bytes()==b'B'*4096
     assert (main/rel).read_bytes()==(src/rel).read_bytes()
    # Known path list: no discovery or project semantics in the following alternatives.
    scratch=case/'scratch';scratch.mkdir()
    def selected():
     dst=scratch/'delta';dst.mkdir()
     for rel in paths:
      s=work/rel;d=dst/rel;d.parent.mkdir(parents=True,exist_ok=True)
      before=identity(s);clone(s,d);assert identity(s)==before
     (dst/'manifest.json').write_text(json.dumps({'base':'fixture-base','replaced':paths,'deleted':[]})+'\n')
    measure('prototype_selected_clone_validate',selected)
    for rel in paths:assert (scratch/'delta'/rel).read_bytes()==b'B'*4096
    measure('prototype_delta_seal_publish',lambda:(seal(scratch/'delta',False),os.rename(scratch/'delta',scratch/'delta-state'),flags(scratch/'delta-state',2)))
    # Materializing the delta still needs a complete tree when a caller requests a folder.
    def materialize():
     clone(main,scratch/'materialized')
     for rel in paths:
      d=scratch/'materialized'/rel;d.unlink();clone(scratch/'delta-state'/rel,d);flags(d,0)
    measure('prototype_delta_materialize',materialize)
    assert sum(len(fs) for _,_,fs in os.walk(scratch/'materialized'))==n
    for rel in paths:assert (scratch/'materialized'/rel).read_bytes()==b'B'*4096
    # Compare two publication choices from the SAME prepared complete index.
    for variant in (['copy','promote'] if trial%2==0 else ['promote','copy']):
     clone(work,scratch/'index')  # Index construction is excluded, just as after real add.
     if variant=='copy':
      measure('prototype_full_clone_seal',lambda:(clone(scratch/'index',scratch/'state'),seal(scratch/'state')))
      measure('prototype_old_index_delete',lambda:erase(scratch/'index'))
     else:
      measure('prototype_full_index_seal_promote',lambda:(seal(scratch/'index',False),os.rename(scratch/'index',scratch/'state'),flags(scratch/'state',2)))
     assert sum(len(fs) for _,_,fs in os.walk(scratch/'state'))==n
     for rel in paths:assert (scratch/'state'/rel).read_bytes()==b'B'*4096
     erase(scratch/'state')
    erase(scratch);run(main,'branch','-q','-D',name)
   report.setdefault('verified',[]).append({'files':n,'changed':changed});save()
  erase(case)
 report['complete']=True;save()
finally:
 save();call(['hdiutil','detach',mount])
image.unlink()
