#!/usr/bin/env python3
"""Verify index promotion, database failure, and process death on an owned APFS store."""
import argparse,json,os,signal,sqlite3,subprocess,tempfile,time,shutil
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',required=True);p.add_argument('--root',required=True);a=p.parse_args()
binary=str(Path(a.binary).resolve());root=Path(tempfile.mkdtemp(prefix='apfs-index-',dir=Path(a.root).resolve()))
env=dict(os.environ,LAYR_ROOT=str(root/'store'),LAYR_AUTHOR_NAME='Test',LAYR_AUTHOR_EMAIL='test@example.invalid',LAYR_TRACE='1')
for key in ('LAYR_SOCKET','LAYR_PROJECT','LAYR_LINE'):env.pop(key,None)
def run(cwd,*args,ok=True):
 r=subprocess.run([binary,*map(str,args)],cwd=cwd,env=env,capture_output=True,text=True)
 assert (r.returncode==0)==ok,(args,r.returncode,r.stdout,r.stderr)
 return r
try:
 src=root/'source';src.mkdir()
 for i in range(10000):
  f=src/f'd{i//100}'/f'f{i}';f.parent.mkdir(exist_ok=True);f.write_bytes(b'original\n')
 run(root,'init','test','--from',src,'-q');project=root/'store/projects/test';main=project/'lines/main'
 run(main,'branch','-q','topic');work=main.parent/'topic';path=work/'d0/f0'
 db=project/'meta/db/layr.sqlite';journal=project/'meta/apfs-index-commit.json'
 def stage():
  paths=[p for p in (project/'meta/stage').iterdir() if p.is_dir()];assert len(paths)==1,paths;return paths[0]
 old_head=run(work,'rev-parse','HEAD').stdout.strip();path.write_bytes(b'staged\n');run(work,'add','d0/f0')
 index=stage();ino=index.stat().st_ino
 with sqlite3.connect(db) as c:
  c.execute("CREATE TRIGGER fail_commit BEFORE INSERT ON records WHEN NEW.op='commit' BEGIN SELECT RAISE(ABORT, 'forced commit failure'); END")
 failed=run(work,'commit','-qm','must fail',ok=False);assert 'forced commit failure' in failed.stderr,failed.stderr
 assert stage().stat().st_ino==ino and (stage()/'d0/f0').read_bytes()==b'staged\n'
 assert run(work,'rev-parse','HEAD').stdout.strip()==old_head and not journal.exists()
 run(work,'fsck');print('PASS database failure restores the same staged tree',flush=True)
 with sqlite3.connect(db) as c:c.execute('DROP TRIGGER fail_commit')
 # Kill after the rename and before the record. No test hook changes product behavior.
 proc=subprocess.Popen([binary,'commit','-qm','interrupted'],cwd=work,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
 stopped=False
 try:
  deadline=time.monotonic()+20
  while time.monotonic()<deadline and proc.poll() is None:
   if journal.exists():
    j=json.loads(journal.read_text())
    if (project/'states'/j['state']).exists():
     os.kill(proc.pid,signal.SIGSTOP);stopped=True;break
   time.sleep(.001)
  assert stopped,'commit finished before the test could stop it'
  with sqlite3.connect(db) as c:assert c.execute('SELECT count(*) FROM states WHERE id=?',(j['state'],)).fetchone()[0]==0
 finally:
  if proc.poll() is None:proc.kill()
  proc.communicate(timeout=20)
 assert journal.exists()
 st=run(work,'status','--porcelain');assert st.stdout=='M  d0/f0\n',st.stdout
 assert stage().stat().st_ino==ino and (stage()/'d0/f0').read_bytes()==b'staged\n'
 assert run(work,'rev-parse','HEAD').stdout.strip()==old_head and not journal.exists()
 run(work,'fsck');print('PASS process death restores the index on the next command',flush=True)
 committed=run(work,'commit','-qm','success')
 assert 'APFS promote index' in committed.stderr and 'APFS clone ' not in committed.stderr,committed.stderr
 with sqlite3.connect(db) as c:head=c.execute("SELECT head FROM lines WHERE name='topic'").fetchone()[0]
 state=project/'states'/head
 assert state.stat().st_ino==ino and (state/'d0/f0').read_bytes()==b'staged\n'
 assert not index.exists() and not journal.exists();assert not run(work,'status','--porcelain').stdout
 assert (main/'d0/f0').read_bytes()==b'original\n';run(work,'fsck')
 print('PASS commit publishes the original index inode without a tree clone',flush=True)
 # A staged tree equal to HEAD must survive a refused empty commit.
 path.write_bytes(b'other\n');run(work,'add','d0/f0');path.write_bytes(b'staged\n');run(work,'add','d0/f0')
 ino=stage().stat().st_ino;run(work,'commit','-qm','empty',ok=False)
 assert stage().stat().st_ino==ino and not journal.exists();assert not run(work,'status','--porcelain').stdout
 run(work,'fsck');print('PASS empty commit keeps the original index',flush=True)
finally:
 subprocess.run(['chflags','-R','nouchg',str(root)],check=True);shutil.rmtree(root)
