#!/usr/bin/env python3
"""Paired release-binary measurements of the actual incremental object writer."""
import argparse, hashlib, json, os, pathlib, platform, random, shutil, statistics, subprocess, tempfile
P=pathlib.Path(__file__).resolve().parent
ap=argparse.ArgumentParser()
ap.add_argument('--before',required=True,type=pathlib.Path)
ap.add_argument('--after',required=True,type=pathlib.Path)
ap.add_argument('--repeats',type=int,default=7)
args=ap.parse_args()
bins={'before':args.before.resolve(),'after':args.after.resolve()}
out={'environment':{'platform':platform.platform(),'cpu':subprocess.check_output(['sysctl','-n','machdep.cpu.brand_string'],text=True).strip(),
'threads':4,'base':'a06d674','binary_sha256':{k:hashlib.sha256(v.read_bytes()).hexdigest() for k,v in bins.items()}},'runs':[]}
for dirs,per in [(10,100),(100,100),(1,10000)]:
 for repeat in range(args.repeats):
  modes=['before','after'];random.Random(1000+repeat).shuffle(modes);expected=None
  for mode in modes:
   parent=pathlib.Path(tempfile.mkdtemp(prefix='layr-v2-perf-'))
   try:
    env=os.environ.copy();env['RAYON_NUM_THREADS']='4'
    row=json.loads(subprocess.check_output([str(bins[mode]),str(parent/'run'),str(dirs),str(per)],env=env,text=True))
    ids=(row['tree'],row['times'])
    if expected is None:expected=ids
    assert ids==expected and row['verified']
    row.update(mode=mode,repeat=repeat);out['runs'].append(row)
    (P/'results.json').write_text(json.dumps(out,indent=2)+'\n')
    print(dirs,per,repeat,mode,round(statistics.median(row['samples_ms']),3),'ms',flush=True)
   finally:shutil.rmtree(parent)
summary=[]
for dirs,per in [(10,100),(100,100),(1,10000)]:
 row={'dirs':dirs,'files':dirs*per}
 for mode in ['before','after']:
  runs=[r for r in out['runs'] if r['dirs']==dirs and r['files']==dirs*per and r['mode']==mode]
  medians=[statistics.median(r['samples_ms']) for r in runs]
  row[mode+'_ms']=statistics.median(medians)
  row[mode+'_range']=[min(medians),max(medians)]
 row['speedup']=row['before_ms']/row['after_ms'];summary.append(row)
(P/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
