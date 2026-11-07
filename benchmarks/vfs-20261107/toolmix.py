import json,sys,os,re,collections,glob,time
files=[f for f in glob.glob(os.path.expanduser('~/.claude/projects/*/*.jsonl'))+glob.glob(os.path.expanduser('~/.claude/projects/*/*/subagents/*.jsonl')) if time.time()-os.path.getmtime(f)<14*86400]
tools=collections.Counter(); bash=collections.Counter()
WALK={'rg','grep','find','fd','ls','tree','du','egrep','ugrep','ag'}
GIT_WALK={'status','diff','add','ls-files','grep','log','show','blame'}
BUILD={'cargo','pnpm','npm','npx','node','tsc','vite','vitest','just','make','python3','python','swift','xcodebuild','bun','go','pytest'}
READ={'cat','head','tail','sed','wc','awk','nl','less'}
def cls(cmd):
    out=[]
    for seg in re.split(r'\|\||&&|;|\||\n',cmd):
        w=seg.strip().split()
        while w and ('=' in w[0] and not w[0].startswith('-')): w=w[1:]
        if not w: continue
        c=os.path.basename(w[0])
        if c in('cd','echo','export','set','true','sleep','for','do','done','if','then','fi','test','['):continue
        if c=='git': out.append('git '+(w[1] if len(w)>1 else '') if (len(w)>1 and w[1] in GIT_WALK) else 'git other')
        elif c=='layr': out.append('layr')
        elif c in WALK: out.append('walk:'+c)
        elif c in BUILD: out.append('build:'+c)
        elif c in READ: out.append('read:'+c)
        else: out.append('other')
    return out
for f in files:
    for line in open(f,errors='ignore'):
        if '"tool_use"' not in line: continue
        try: d=json.loads(line)
        except: continue
        for c in (d.get('message',{}).get('content') or []):
            if isinstance(c,dict) and c.get('type')=='tool_use':
                n=c['name']; tools[n]+=1
                if n=='Bash':
                    for k in set(cls(c.get('input',{}).get('command',''))): bash[k]+=1
print('files',len(files),'tool calls',sum(tools.values()))
for n,k in tools.most_common(14): print(f'  {n:28}{k:7}')
print('bash commands by class (a call can count in several):')
agg=collections.Counter()
for k,v in bash.items(): agg[k.split(':')[0] if ':' in k else ('git-walk' if k.startswith('git ') and k!='git other' else k)]+=v
for k,v in agg.most_common(): print(f'  {k:28}{v:7}')
for k,v in bash.most_common(25): print(f'    {k:26}{v:7}')
