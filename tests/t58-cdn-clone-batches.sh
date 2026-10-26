#!/bin/bash
# The clone caller batches both object classes and does not fetch old parent trees twice.
. "$(dirname "$0")/lib.sh"
set -e
export CDN_CLONE_TEST_ROOT=$T CDN_CLONE_TEST_BINARY=$LAYR
CDN_CLONE_TEST_SCRIPT=$(cd "$(dirname "$0")" && pwd)/cdn_clone.py
mkdir -p "$T/source"
git -C "$T/source" init -q -b main
python3 - <<'PY'
import os,pathlib,subprocess
root=pathlib.Path(os.environ['CDN_CLONE_TEST_ROOT'])/'source'
def git(*args,body=None):return subprocess.check_output(['git','-C',str(root),*args],input=body).decode().strip()
(root/'payload').write_text('base\n');git('add','.');base=git('commit-tree',git('write-tree'),body=b'base\n');parents=[]
for n in range(129):
 (root/'payload').write_text('version '+str(n)+'\n');git('add','.');tree=git('write-tree')
 parents.append(git('commit-tree',tree,'-p',base,body=('branch '+str(n)+'\n').encode()))
tip=git('commit-tree',tree,*[part for parent in parents for part in ['-p',parent]],body=b'merge all branches\n')
git('update-ref','refs/heads/main',tip);git('reset','--hard','-q','main')
assert int(git('rev-list','--count','main'))==131
PY
l git clone "$T/source" source -q >/dev/null
cd "$LAYR_ROOT/projects/source/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q
new_fs "$T/replica" 8G
python3 - <<'PYTRUST'
import json,os,subprocess
binary=os.environ['CDN_CLONE_TEST_BINARY'];root=os.environ['CDN_CLONE_TEST_ROOT']
machine=json.loads(subprocess.check_output([binary,'remote','self','--json'],env=dict(os.environ,LAYR_ROOT=root+'/replica')))
subprocess.run([binary,'remote','trust','archive',machine['id'],'--public-key',machine['public_key']],check=True,stdout=subprocess.DEVNULL)
subprocess.run([binary,'push','archive','-q'],check=True)
PYTRUST
export LAYR_ROOT=$T/replica
unset LAYR_PAPER_KEY_FILE
unset LAYR_PROJECT LAYR_LINE
cd "$T"
python3 "$CDN_CLONE_TEST_SCRIPT"
PASS=$((PASS+1))
finish
