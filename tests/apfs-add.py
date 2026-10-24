#!/usr/bin/env python3
"""Compare selective APFS add with the full snapshot implementation."""
import argparse, mmap, os, shutil, subprocess, tempfile
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',required=True);p.add_argument('--baseline',required=True);p.add_argument('--root',required=True);a=p.parse_args()
root=Path(tempfile.mkdtemp(prefix='apfs-add-',dir=Path(a.root).resolve()))
binaries=[str(Path(a.baseline).resolve()),str(Path(a.binary).resolve())]
try:
 src=root/'source';src.mkdir()
 for name in ['one','two','readonly','remove','dir/old','old-dir/file']:
  f=src/name;f.parent.mkdir(exist_ok=True);f.write_bytes(b'original\n')
 (src/'.gitignore').write_text('ignored\n');(src/'readonly').chmod(0o444)
 envs=[];works=[]
 def run(i,*args,ok=True):
  r=subprocess.run([binaries[i],*args],cwd=works[i],env=envs[i],capture_output=True,text=True)
  assert (r.returncode==0)==ok,(i,args,r.returncode,r.stdout,r.stderr)
  return r
 for i in range(2):
  envs.append(dict(os.environ,LAYR_ROOT=str(root/f'store{i}'),LAYR_TRACE='1',LAYR_AUTHOR_NAME='Test',LAYR_AUTHOR_EMAIL='test@example.invalid'))
  for key in ('LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE'):envs[i].pop(key,None)
  works.append(root);run(i,'init','test','--from',str(src),'-q');works[i]=root/f'store{i}/projects/test/lines/main'
  run(i,'line','new','-q','topic');works[i]=works[i].parent/'topic'
 def same(label,*args,ok=True,fallback=False):
  before=run(0,*args,ok=ok);after=run(1,*args,ok=ok)
  assert before.stdout==after.stdout,(label,before.stdout,after.stdout)
  if ok:
   assert 'APFS validated read' in after.stderr,(label,after.stderr)
   assert ('APFS copy selected' in after.stderr)!=fallback,(label,after.stderr)
  for command in [('status','--porcelain','--ignored'),('diff','--cached'),('diff',)]:
   assert run(0,*command).stdout==run(1,*command).stdout,(label,command)
  print('PASS',label,flush=True)
 for w in works:
  f=w/'one';s=f.stat();f.write_bytes(b'changed!\n');os.utime(f,ns=(s.st_atime_ns,s.st_mtime_ns))
  (w/'two').write_bytes(b'unselected\n');(w/'ignored').write_bytes(b'ignored\n')
 same('dry run preserves the index','add','-n','one')
 same('selected file and restored nanosecond time','add','one')
 same('missing path leaves staged bytes intact','add','missing',ok=False)
 same('ignored path reports failure','add','ignored',ok=False)
 same('forced ignored path','add','-f','ignored')
 for w in works:
  (w/'remove').unlink();(w/'new').write_bytes(b'new\n');(w/'readonly').chmod(0o644);(w/'readonly').write_bytes(b'readonly\n');(w/'readonly').chmod(0o444)
 same('update excludes new files','add','-u')
 for w in works:
  shutil.rmtree(w/'dir');(w/'dir').symlink_to('two');os.rename(w/'old-dir',w/'renamed')
 same('directory to link and directory rename','add','-A')
 for w in works:
  (w/'dir').unlink();(w/'dir').mkdir();(w/'dir/new').write_bytes(b'new directory\n')
 same('link to directory','add','-A')
 for i in range(2):run(i,'save','-qm','selected')
 # A live mmap has no generation proof and must retain the snapshot path.
 handles=[];maps=[]
 try:
  for w in works:
   f=(w/'one').open('r+b');handles.append(f);m=mmap.mmap(f.fileno(),0);maps.append(m);m[0:1]=b'Z';m.flush()
  same('mmap falls back to snapshots','add','one',fallback=True)
 finally:
  for m in maps:m.close()
  for f in handles:f.close()
 for i in range(2):run(i,'save','-qm','mapped');run(i,'fsck')
 print('PASS both histories pass fsck',flush=True)
finally:
 subprocess.run(['chflags','-R','nouchg',str(root)],check=True);shutil.rmtree(root)
