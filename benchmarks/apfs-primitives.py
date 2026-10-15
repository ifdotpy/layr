#!/usr/bin/env python3
"""Measure APFS primitives on disposable fixtures, not complete layr snapshots."""
import argparse,ctypes,json,os,pathlib,platform,shutil,time
from concurrent.futures import ThreadPoolExecutor
p=argparse.ArgumentParser();p.add_argument('--root',required=True);p.add_argument('--files',type=int,default=10000);p.add_argument('--repeats',type=int,default=3);a=p.parse_args()
root=pathlib.Path(a.root);root.mkdir(parents=True,exist_ok=False);src=root/'source';src.mkdir()
for i in range(a.files):
 f=src/f'd{i//100:05}'/f'f{i:07}';f.parent.mkdir(exist_ok=True);f.write_bytes((f'{i:07} abcdef\n'.encode()*400)[:4096])
lib=ctypes.CDLL(None,use_errno=True)
lib.clonefile.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_int]
lib.copyfile.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_void_p,ctypes.c_uint32]
def check(rc):
 if rc: raise OSError(ctypes.get_errno(),os.strerror(ctypes.get_errno()))
def files(): return list(src.glob('*/*'))
paths=files();dirs=sorted(src.iterdir());rows=[]
def measure(label,fn,trial):
 start=time.perf_counter();cpu=time.process_time();fn();r={'trial':trial,'operation':label,'wall':time.perf_counter()-start,'cpu':time.process_time()-cpu,'load':os.getloadavg()};rows.append(r);print(json.dumps(r),flush=True)
def clone_files(dst,workers):
 dst.mkdir()
 for d in dirs: (dst/d.name).mkdir()
 def clone(f): check(lib.clonefile(os.fsencode(f),os.fsencode(dst/f.relative_to(src)),2))
 if workers==1:
  for f in paths: clone(f)
 else:
  with ThreadPoolExecutor(workers) as pool: list(pool.map(clone,paths))
for trial in range(a.repeats):
 measure('walk_stat',lambda:[f.stat() for d in src.iterdir() for f in d.iterdir()],trial)
 measure('read_all',lambda:[f.read_bytes() for f in paths],trial)
 for method in ['clone_1','clone_4','clone_8','copyfile_recursive','clone_directory']:
  dst=root/f'{method}-{trial}'
  if method.startswith('clone_') and method.split('_')[1].isdigit(): fn=lambda:clone_files(dst,int(method.split('_')[1]))
  elif method=='copyfile_recursive': fn=lambda:check(lib.copyfile(os.fsencode(src),os.fsencode(dst),None,15|(1<<15)|(1<<24)|(1<<18)|(1<<19)))
  else: fn=lambda:check(lib.clonefile(os.fsencode(src),os.fsencode(dst),2))
  measure(method,fn,trial)
  assert sum(1 for _ in dst.glob('*/*'))==a.files
  for f in (paths[0],paths[-1]): assert (dst/f.relative_to(src)).read_bytes()==f.read_bytes()
  shutil.rmtree(dst)
(root/'results.json').write_text(json.dumps({'platform':platform.platform(),'files':a.files,'rows':rows,'note':'Clones omit layr origin markers, layer exclusion, source validation and immutable flags.'},indent=2))
