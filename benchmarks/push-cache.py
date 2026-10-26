#!/usr/bin/env python3
"""Reproducible push-cache comparison (run as root in an owned Linux VM).

Prepare WORKSPACE/bench as a Btrfs mount with at least 12 GiB free (48 GiB used
for the recorded run). Put independent release executables in layr-before and
layr-after, and Git source copies pinned to c1d7229 / eec4c6d on branch main in
layr-source / studio-source. All paths are relative to WORKSPACE.

Run: python3 benchmarks/push-cache.py --workspace WORKSPACE before
     python3 benchmarks/push-cache.py --workspace WORKSPACE after
     python3 benchmarks/push-cache.py --workspace WORKSPACE daily

A fresh after comparison can use --run-id NAME for both after and daily. Keep
separate Cargo target directories for the two source trees; copying older source mtimes can otherwise reuse a stale executable.
The harness checks distinct binary hashes and expected cache formats. It writes
JSONL measurements and sanitized request traces, never keys or token values.
The daily TLS stand-in follows tests/t46-machine-bootstrap.sh with 5 ms/request.
"""
import os,pathlib,subprocess,json,time,hashlib
import argparse,atexit,signal
parser=argparse.ArgumentParser(description='Compare push caches against two pinned Git fixture copies on a Btrfs volume.')
parser.add_argument('--workspace',type=pathlib.Path,default=pathlib.Path('/root/pc'))
parser.add_argument('--run-id',default='',help='Suffix for fresh before/after archives when repeating the comparison.')
parser.add_argument('--after-run-id',default='',help='Use a fresh after archive with an existing matching baseline run.')
parser.add_argument('--fixture',choices=['all','layr','studio'],default='all')
parser.add_argument('--resume',action='store_true',help='Resume an interrupted after push; report log timestamp estimate separately.')
parser.add_argument('phase',choices=['before','after','daily'])
args=parser.parse_args()
active_process=None
pid_file=os.environ.get('LAYR_PUSH_GUARD_PID_FILE')
if pid_file:
    pathlib.Path(pid_file).write_text(str(os.getpid()))
    atexit.register(lambda:pathlib.Path(pid_file).unlink(missing_ok=True))
def terminate(signum,frame):
    if active_process is not None:
        active_process.terminate()
        try:active_process.wait(timeout=10)
        except subprocess.TimeoutExpired:active_process.kill();active_process.wait()
    raise SystemExit(128+signum)
signal.signal(signal.SIGTERM,terminate)
signal.signal(signal.SIGINT,terminate)
base=args.workspace.resolve()
fs=base/'bench'
results=base/'push-cache-results.jsonl'
assert os.path.ismount(fs), 'benchmark Btrfs mount is missing; remount the fixture volume before running'
assert subprocess.check_output(['findmnt','-n','-o','FSTYPE',str(fs)],text=True).strip()=='btrfs', 'benchmark requires Btrfs'
if args.phase in ('after','daily'):
    assert hashlib.sha256((base/'layr-before').read_bytes()).digest()!=hashlib.sha256((base/'layr-after').read_bytes()).digest(), 'before/after binaries are identical; force separate builds'
def label_for(phase):
    label=args.after_run_id if phase=='after' and args.after_run_id else args.run_id
    return phase+('-'+label if label else '')
def archive_for(kind,phase):return fs/(kind+'-'+label_for(phase)+'-archive')

def run(args,env=None,cwd=None,label=None):
    global active_process
    if pathlib.Path(args[0])==base/'layr-after':
        binary=(base/'layr-after').read_bytes()
        assert b'pushed-v2.json.zst' in binary, 'new cache marker missing from after binary'
        assert hashlib.sha256(binary).digest()!=hashlib.sha256((base/'layr-before').read_bytes()).digest(), 'after binary matches baseline'
    started=time.monotonic()
    with (base/(label+'.log' if label else 'command.log')).open('ab') as log:
        active_process=subprocess.Popen([str(x) for x in args],env=env,cwd=cwd,stdout=log,stderr=subprocess.STDOUT)
        try:status=active_process.wait()
        finally:active_process=None
        if status:raise subprocess.CalledProcessError(status,args)
    return time.monotonic()-started

def size(p):
    paths=[x for x in p.rglob('*') if x.is_file()] if p.is_dir() else [p]
    return sum(x.stat().st_size for x in paths)

def measure(kind,phase,binary):
    root=fs/(kind+'-local')
    project=root/'projects'/kind
    line=project/'lines/main'
    archive=archive_for(kind,phase)
    env=os.environ|{'LAYR_ROOT':str(root),'LAYR_AUTHOR_NAME':'Cache benchmark','LAYR_AUTHOR_EMAIL':'cache@example.test','LAYR_TRACE':'1'}
    source=base/('layr-source' if kind=='layr' else 'studio-source')
    if phase=='after':
        assert project.exists() and archive_for(kind,'before').exists(), 'original baseline fixture is missing'
    if not project.exists():
        root.mkdir(exist_ok=True)
        seconds=run([base/'layr-before','git','clone',source,kind,'-q'],env,cwd=base,label=kind+'-import')
        print(json.dumps({'phase':'import','fixture':kind,'seconds':seconds}),flush=True)
    # Leave source snapshots unchanged; a second configured archive gets its own cache directory.
    before={}
    prefix_seconds=None
    if args.resume:
        assert phase=='after' and archive.exists()
        log=base/(kind+'-'+label_for(phase)+'.log')
        birth=int(subprocess.check_output(['stat','-c','%W',str(log)]))
        prefix_seconds=log.stat().st_mtime-birth
        before={str(p.relative_to(archive)):hashlib.sha256(p.read_bytes()).hexdigest() for p in archive.rglob('*') if p.is_file()}
    else:
        run([binary,'remote','add',label_for(phase),'file://'+str(archive),'--paper-key-out',base/(kind+'-'+label_for(phase)+'-paper')],env,cwd=line)
    seconds=run([binary,'push',label_for(phase),'-q'],env,cwd=line,label=kind+'-'+label_for(phase))
    target=project/'meta/remote-stores'/hashlib.sha256(('file://'+str(archive)).encode()).hexdigest()
    files=[p for p in target.rglob('*') if p.is_file() and (p.parent.name=='pushed' or p.name in ('pushed.json','pushed-v2.json.zst','push-cache.sqlite3'))]
    assert (target/'pushed-v2.json.zst').is_file() if phase=='after' else (target/'pushed.json').is_file(), 'unexpected cache format'
    result={'after_run_id':args.after_run_id,'run_id':args.run_id,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'fixture':kind,'phase':phase,'seconds':seconds,'snapshot_states':sum(1 for p in (project/'states').iterdir() if p.is_dir()),'commits':int(subprocess.check_output(['git','-C',str(source),'rev-list','--count','HEAD'])),'uploaded_states':sum(1 for p in (archive/'states').rglob('*.tree.age')),'cache_bytes':sum(p.stat().st_size for p in files),'cache_allocated_bytes':sum(p.stat().st_blocks*512 for p in files),'cache_files':len(files),'encrypted_store_bytes':size(archive),'cache_path':str(target),'source_tip':subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD']).decode().strip(),'time_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())}
    result['encrypted_store_objects']=sum(1 for p in archive.rglob('*') if p.is_file())
    result['cache_percent']=100*result['cache_bytes']/result['encrypted_store_bytes']
    if args.resume:
        for name,digest in before.items():assert hashlib.sha256((archive/name).read_bytes()).hexdigest()==digest,name
        result.update(interrupted=True,resume_seconds=seconds,prefix_last_progress_seconds=prefix_seconds,seconds_estimated=prefix_seconds+seconds,seconds=None,preserved_objects=len(before))
    if phase=='after':
        references=[json.loads(line) for line in results.read_text().splitlines() if line]
        reference=next(row for row in reversed(references) if row['fixture']==kind and row['phase']=='before' and row.get('run_id','')==args.run_id)
        assert result['uploaded_states']==reference['uploaded_states'], result
        assert result['encrypted_store_bytes']<=reference['encrypted_store_bytes']*1.03, result
        assert result['cache_percent']<10, result
    with results.open('a') as out:out.write(json.dumps(result)+'\n')
    print(json.dumps(result),flush=True)


def daily():
    rows=[json.loads(line) for line in results.read_text().splitlines() if line]
    assert any(row['fixture']=='studio' and row['phase']=='after' and row.get('run_id','')==args.run_id and row.get('after_run_id','')==args.after_run_id and not row.get('interrupted') for row in rows), 'daily comparison requires a completed uninterrupted large after run'
    # Serve the exact file-store objects through the private TLS routes. Reflink each
    # corresponding warm cache to the URL alias, so the measured push starts with the
    # same local state as its preceding complete push, without repeating bulk transfer.
    import base64,collections,http.server,secrets,ssl,threading,urllib.parse
    root=fs/'studio-local';project=root/'projects/studio';line=project/'lines/main'
    tls=base/'tls';tls.mkdir(exist_ok=True)
    def openssl(command):
        subprocess.run(['openssl',*command],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(tls/'ca-key.pem'),'-out',str(tls/'ca.pem'),'-days','1','-subj','/CN=Push cache benchmark CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign'])
    openssl(['req','-new','-newkey','rsa:2048','-nodes','-keyout',str(tls/'key.pem'),'-out',str(tls/'request.csr'),'-subj','/CN=localhost'])
    (tls/'extensions.cnf').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n')
    openssl(['x509','-req','-in',str(tls/'request.csr'),'-CA',str(tls/'ca.pem'),'-CAkey',str(tls/'ca-key.pem'),'-CAcreateserial','-out',str(tls/'cert.pem'),'-days','1','-extfile',str(tls/'extensions.cnf')])
    grant=secrets.token_hex(32);proof='v1.read.push-cache-fixture'
    grant_path=tls/'grant';grant_path.write_text(grant);grant_path.chmod(0o600)
    requests=[];errors=[]
    class Gateway(http.server.BaseHTTPRequestHandler):
        def log_message(self,*args): pass
        def reply(self,status,body):
            body=body if isinstance(body,bytes) else json.dumps(body).encode()
            self.send_response(status);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
        def do_GET(self): self.dispatch()
        def do_POST(self): self.dispatch()
        def do_PUT(self): self.dispatch()
        def dispatch(self):
            try:self.api()
            except Exception as error:
                errors.append(type(error).__name__+': '+str(error));self.reply(500,{})
        def api(self):
            time.sleep(.005)
            parsed=urllib.parse.urlsplit(self.path);parts=parsed.path.split('/')
            if parts[:2]==['','p']:
                assert parts[2] in ('before','after') and self.headers.get('Authorization') is None
                requests.append({'method':self.command,'phase':parts[2],'path':self.path,'credential':'none'})
                self.reply(404,{});return
            assert parts[:3]==['','v1','p'] and parts[3] in ('before','after')
            phase=parts[3];action='/'.join(parts[4:]);archive=archive_for('studio',phase)
            auth=self.headers.get('Authorization');query=urllib.parse.parse_qs(parsed.query)
            kind='grant' if auth=='Bearer '+grant else 'read' if auth=='Bearer '+proof else 'none'
            requests.append({'method':self.command,'phase':phase,'path':self.path,'credential':kind})
            if self.command=='GET' and (action=='store.json' or action.startswith('slots/machine-')):
                assert kind=='grant';path=archive/action
                self.reply(200,path.read_bytes()) if path.exists() else self.reply(404,{})
                return
            if self.command=='GET' and action=='challenge':
                assert kind=='grant'
                self.reply(200,{'challenge':base64.b64encode(next((archive/'records').rglob('*.age')).read_bytes()).decode()});return
            if self.command=='POST' and action=='token':
                assert kind=='grant'
                body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
                assert isinstance(body.get('challenge'),str) and '"machine"' in body['challenge']
                self.reply(200,{'token':proof,'expires':int(time.time()*1000)+300000});return
            if self.command=='GET' and action=='objects':
                assert set(query)<= {'prefix','limit','continuation'}
                prefix=query.get('prefix',[''])[0]
                assert kind=='read' or (kind=='grant' and prefix=='keys/')
                # Start at the requested prefix rather than traversing all history for
                # every list call. This preserves the file backend's listing cost.
                directory=archive/prefix
                if not prefix.endswith('/'): directory=directory.parent
                files=sorted([{'key':str(p.relative_to(archive)),'modified':int(p.stat().st_mtime*1000)} for p in directory.rglob('*') if p.is_file() and str(p.relative_to(archive)).startswith(prefix)],key=lambda row:row['key'])
                offset=int(query.get('continuation',['0'])[0]);limit=int(query.get('limit',['1000'])[0])
                assert offset>=0 and 0<limit<=1000
                page=files[offset:offset+limit]
                self.reply(200,{'objects':page,'continuation':str(offset+limit) if offset+limit<len(files) else None});return
            if self.command=='GET' and action.startswith('objects/'):
                assert kind=='read';name=action[len('objects/'):];assert '..' not in pathlib.PurePosixPath(name).parts
                path=archive/name
                self.reply(200,path.read_bytes()) if path.is_file() else self.reply(404,{})
                return
            if self.command=='PUT' and action.startswith('objects/'):
                assert kind=='grant';name=action[len('objects/'):];assert '..' not in pathlib.PurePosixPath(name).parts
                assert name.startswith(('records/','states/','index/','slots/','keys/'))
                body=self.rfile.read(int(self.headers.get('Content-Length','0')));path=archive/name
                if path.exists():self.reply(412,b'');return
                path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(body);self.reply(201,b'');return
            raise AssertionError((self.command,action,kind))
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Gateway)
    context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(tls/'cert.pem',tls/'key.pem')
    server.socket=context.wrap_socket(server.socket,server_side=True)
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    env=os.environ|{'LAYR_ROOT':str(root),'LAYR_AUTHOR_NAME':'Cache benchmark','LAYR_AUTHOR_EMAIL':'cache@example.test','LAYR_GATEWAY_CA_FILE':str(tls/'ca.pem'),'LAYR_GATEWAY_TOKEN_FILE':str(grant_path),'LAYR_TRACE':'1'}
    try:
        for phase in ['before','after']:
            url=f'layr+https://localhost:{server.server_port}/{phase}'
            run([base/'layr-after','remote','add','gateway-'+label_for(phase),url],env,cwd=line,label='daily-setup')
            source=project/'meta/remote-stores'/hashlib.sha256(('file://'+str(archive_for('studio',phase))).encode()).hexdigest()
            target=project/'meta/remote-stores'/hashlib.sha256(url.encode()).hexdigest()
            target.mkdir(exist_ok=True)
            subprocess.run(['cp','-a','--reflink=always',str(source)+'/.',str(target)],check=True)
        prior_states=sum(1 for p in (project/'states').iterdir() if p.is_dir())
        (line/'push-cache-daily.txt').write_text(f'Daily comparison {args.run_id} after {prior_states} snapshots.\n')
        run([base/'layr-after','add','push-cache-daily.txt'],env,cwd=line,label='daily-setup')
        run([base/'layr-after','save','-qm','Push cache daily benchmark'],env,cwd=line,label='daily-setup')
        for phase in ['before','after']:
            requests.clear();started=time.monotonic()
            seconds=run([base/('layr-'+phase),'push','gateway-'+label_for(phase),'-q'],env,cwd=line,label='daily-'+phase)
            assert not errors,errors
            counts=dict(collections.Counter(r['method'] for r in requests))
            listing_prefixes=[urllib.parse.parse_qs(urllib.parse.urlsplit(r['path']).query).get('prefix',[''])[0] for r in requests if '/objects?' in r['path']]
            result={'after_run_id':args.after_run_id,'run_id':args.run_id,'binary_sha256':hashlib.sha256((base/('layr-'+phase)).read_bytes()).hexdigest(),'fixture':'studio','phase':'daily-'+phase,'seconds':seconds,'imported_commits':7965,'prior_states':prior_states,'requests':counts,'request_count':len(requests),'listing_prefixes':listing_prefixes,'delay_ms':5,'cache_warmup':'same-object file-store cache reflinked to gateway URL alias','time_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())}
            with results.open('a') as out:out.write(json.dumps(result)+'\n')
            (base/('daily-'+phase+'-requests.json')).write_text(json.dumps(requests,indent=2)+'\n')
            print(json.dumps(result),flush=True)
    finally:
        (base/'daily-last-requests.json').write_text(json.dumps(requests,indent=2)+'\n')
        (base/'daily-last-errors.json').write_text(json.dumps(errors,indent=2)+'\n')
        server.shutdown();server.server_close();thread.join(timeout=10)

if args.phase=='daily':daily()
else:
    for fixture in (['layr','studio'] if args.fixture=='all' else [args.fixture]):measure(fixture,args.phase,base/('layr-'+args.phase))
