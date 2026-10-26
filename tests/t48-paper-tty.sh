#!/bin/bash
# Synthetic paper keys use the controlling terminal, never captured command output.
. "$(dirname "$0")/lib.sh"
set -e
l init project -q
export PAPER_TTY_TEST_ROOT="$T" PAPER_TTY_TEST_BINARY="$LAYR"
python3 - <<'PY'
import errno,fcntl,json,os,pathlib,pty,re,selectors,subprocess,termios,time

root=pathlib.Path(os.environ['PAPER_TTY_TEST_ROOT']);binary=os.environ['PAPER_TTY_TEST_BINARY']
env=dict(os.environ,LAYR_PROJECT='project',LAYR_LINE='main')
for name in ['LAYR_PERSON_KEY_FILE','LAYR_GATEWAY_TOKEN_FILE','LAYR_PAPER_KEY_FILE']:
 env.pop(name,None)
line=pathlib.Path(env['LAYR_ROOT'])/'projects/project/lines/main'
checks=0
def check(value,label):
 global checks
 assert value,label
 checks+=1;print('PASS',label,flush=True)
def run(args):
 return subprocess.run([binary,*args],cwd=line,env=env,capture_output=True,timeout=30)
def objects(path):
 return [p for p in path.rglob('*') if p.is_file()]

p=run(['remote','add','captured','file://'+str(root/'captured'),'--paper-key-tty'])
check(p.returncode!=0 and b'terminal stdin and stdout' in p.stderr,'captured invocation refuses terminal key output')
check(not objects(root/'captured'),'captured invocation publishes no remote objects')
p=run(['remote','add','conflict','file://'+str(root/'conflict'),'--paper-key-tty','--paper-key-out',str(root/'paper')])
check(p.returncode!=0 and b'not both' in p.stderr and not (root/'paper').exists(),'terminal and file output cannot be combined')
p=run(['remote','add','peer','ssh://localhost/project','--paper-key-tty'])
check(p.returncode!=0 and b'store remote' in p.stderr,'terminal paper output requires a store remote')

def terminal_run(name,answer='correct',redirect_in=False,redirect_out=False,control=True,other_input=False):
 master,slave=pty.openpty();out_master,out_slave=pty.openpty()
 archive=root/name;received={'tty':bytearray(),'stdout':bytearray(),'stderr':bytearray()}
 def setup():
  os.setsid()
  if control: fcntl.ioctl(slave,termios.TIOCSCTTY,0)
 p=subprocess.Popen([binary,'remote','add',name,'file://'+str(archive),'--paper-key-tty','--json'],
  cwd=line,env=env,stdin=subprocess.DEVNULL if redirect_in else out_slave if other_input else slave,
  stdout=subprocess.PIPE if redirect_out else out_slave,stderr=subprocess.PIPE,
  preexec_fn=setup,pass_fds=(slave,))
 os.close(slave);os.close(out_slave)
 selector=selectors.DefaultSelector()
 selector.register(master,selectors.EVENT_READ,'tty')
 if redirect_out:
  os.close(out_master);selector.register(p.stdout,selectors.EVENT_READ,'stdout')
 else: selector.register(out_master,selectors.EVENT_READ,'stdout')
 selector.register(p.stderr,selectors.EVENT_READ,'stderr')
 words=None;sent=False;deadline=time.monotonic()+35
 try:
  while selector.get_map():
   remaining=deadline-time.monotonic()
   assert remaining>0,'terminal invocation timed out'
   for key,_ in selector.select(remaining):
    try: data=os.read(key.fd,4096)
    except OSError as error:
     if error.errno!=errno.EIO: raise
     data=b''
    if not data:
     selector.unregister(key.fileobj);continue
    received[key.data].extend(data)
   if not sent and b'Type the final word of the paper key to confirm: ' in received['tty']:
    match=re.search(rb'Paper key \(24 words\):\r?\n([a-z ]+)\r?\n',received['tty'])
    assert match is not None,'terminal must display its paper key before confirmation'
    words=match.group(1)
    check(len(words.split())==24,'the controlling terminal shows exactly 24 paper words')
    check(p.poll() is None and not objects(archive),'confirmation is required before any remote publication')
    reply=words.split()[-1] if answer=='correct' else b'WRONG'
    os.write(master,reply+b'\n');sent=True
  code=p.wait(timeout=5)
 finally:
  selector.close();os.close(master)
  if not redirect_out: os.close(out_master)
  p.stderr.close()
  if p.stdout: p.stdout.close()
  if p.poll() is None: p.kill();p.wait()
 if words:
  check(words not in received['stdout'] and words not in received['stderr'],'paper words never enter stdout or stderr')
  # Only synthetic keys are inspected; none are written to a test log or key file.
  check(all(words not in path.read_bytes() for path in objects(root)),'paper words do not appear in store files, records or logs')
 return code,received,words

for name,settings,expected in [
 ('stdin-pipe',{'redirect_in':True},b'terminal stdin and stdout'),
 ('stdout-pipe',{'redirect_out':True},b'terminal stdin and stdout'),
 ('no-controlling-terminal',{'control':False},b'controlling terminal'),
 ('other-terminal',{'other_input':True},b'stdin on your controlling terminal')]:
 code,received,words=terminal_run(name,**settings)
 check(code!=0 and expected in received['stderr'] and words is None and not objects(root/name),name+' refuses without showing or publishing a key')

code,received,words=terminal_run('wrong-confirmation',answer='wrong')
check(code!=0 and b'confirmation did not match' in received['stderr'] and not objects(root/'wrong-confirmation'),'wrong confirmation aborts creation without publication')
check(run(['config','--get','cloud.remote.wrong-confirmation.url']).returncode!=0,'wrong confirmation records no remote URL')
code,received,words=terminal_run('terminal-store')
check(code==0,'correct confirmation completes store creation')
result=json.loads(received['stdout'])
check(result['recovery_key_file'] is None,'terminal creation reports no paper key file')
pin=json.loads(run(['config','--get','cloud.remote.terminal-store.store']).stdout)
check(pin['paper_recipient'].startswith('age1') and (root/'terminal-store/slots/paper.age').is_file(),'terminal creation pins the public paper recipient and seals its slot')
check(run(['push','terminal-store','-q']).returncode==0,'the owner pushes through its machine slot after terminal creation')
check(words not in run(['op','export']).stdout,'exported records contain no paper words')
# A real clone opens the generated paper slot, using only this synthetic in-memory key.
p=run(['clone','file://'+str(root/'terminal-store'),'paper-copy','--paper-key',words.decode(),'-q'])
check(p.returncode==0 and words not in p.stdout+p.stderr,'the displayed paper key opens the published store slot')
check(all(words not in path.read_bytes() for path in objects(root)),'push and paper-key clone persist no plaintext paper words')
p=run(['remote','add','existing','file://'+str(root/'terminal-store'),'--paper-key-tty'])
check(p.returncode!=0 and b'already exists' in p.stderr,'an existing store cannot print its paper key again')
(root/'paper-tty-counts').write_text(str(checks))
PY
PASS=$((PASS + $(cat "$T/paper-tty-counts")))
finish
