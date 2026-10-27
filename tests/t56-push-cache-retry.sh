#!/bin/bash
# Recover a lost compressed push index while content-addressed cache nodes remain.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init retry --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/retry/lines/main"
l line new topic -q
cd ../topic
printf 'first child\n' > child
l add child
l save -qam first
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q
export RETRY_CACHE=$LAYR_ROOT/projects/retry/meta/remote-stores RETRY_ARCHIVE=$T/archive
python3 - <<'PY'
import hashlib,json,os,pathlib,sqlite3,subprocess
cache=pathlib.Path(os.environ['RETRY_CACHE'])
indexes=list(cache.glob('*/pushed-v2.json.zst'))
assert len(indexes)==1,indexes
index=json.loads(subprocess.check_output(['zstd','-q','-d','-c',str(indexes[0])]))
assert index['format']==2 and index['states']
with sqlite3.connect(indexes[0].parent/'push-cache.sqlite3') as db:
    for entry in index['states'].values():
        assert 0<=entry['depth']<32 and entry['root']
        pending=[entry['root']]
        while pending:
            key=pending.pop()
            row=db.execute('SELECT data FROM nodes WHERE hash=?',(bytes.fromhex(key),)).fetchone()
            assert row is not None,key
            raw=subprocess.check_output(['zstd','-q','-d','-c'],input=row[0])
            assert hashlib.sha256(raw).hexdigest()==key
            node=json.loads(raw)
            pending.extend(node['dirs'].values())
assert not (indexes[0].parent/'push-journal.sqlite3').exists()
indexes[0].unlink()
root=pathlib.Path(os.environ['RETRY_ARCHIVE'])
before={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.glob('states/**/*.tree.age')}
assert before
(root.parent/'before-trees.json').write_text(json.dumps(before))
PY
printf 'second child\n' >> child
l save -qam second
if l push archive -q > "$T/retry-output" 2>&1; then
    echo 'PASS push resumes with complete state caches but no index checkpoint'
    PASS=$((PASS+1))
else
    cat "$T/retry-output"
    echo 'FAIL push resumes with complete state caches but no index checkpoint'
    FAIL=$((FAIL+1))
fi
python3 - <<'PY'
import hashlib,json,os,pathlib
root=pathlib.Path(os.environ['RETRY_ARCHIVE'])
before=json.loads((root.parent/'before-trees.json').read_text())
for path,digest in before.items():
    assert hashlib.sha256((root/path).read_bytes()).hexdigest()==digest,path
print('PASS retry preserves every published immutable tree')
PY
PASS=$((PASS+1))
ok 'retried store records and file objects verify' l fsck --remote archive --files
# A syntactically valid local tree is not proof of the published remote tree.
python3 - <<'PY'
import hashlib,json,os,pathlib,sqlite3,subprocess
changed=0
for index in pathlib.Path(os.environ['RETRY_CACHE']).glob('*/pushed-v2.json.zst'):
    metadata=json.loads(subprocess.check_output(['zstd','-q','-d','-c',str(index)]))
    with sqlite3.connect(index.parent/'push-cache.sqlite3') as db:
        for entry in metadata['states'].values():
            if not entry.get('root'):
                continue
            key=bytes.fromhex(entry['root'])
            packed=db.execute('SELECT data FROM nodes WHERE hash=?',(key,)).fetchone()
            assert packed is not None
            node=json.loads(subprocess.check_output(['zstd','-q','-d','-c'],input=packed[0]))
            assert 'notes.txt' in node['files']
            del node['files']['notes.txt']
            raw=json.dumps(node,separators=(',',':'),ensure_ascii=False).encode()
            wrong_hash=hashlib.sha256(raw).digest()
            wrong_data=subprocess.check_output(['zstd','-q','-c'],input=raw)
            # Leave both a structurally valid mutation at the old address and a
            # self-consistent hash/data pair for the wrong tree. Losing the index
            # must force a remote check even when the leftover graph hashes agree.
            db.execute('UPDATE nodes SET data=? WHERE hash=?',(wrong_data,key))
            db.execute('INSERT OR REPLACE INTO nodes VALUES (?,?)',(wrong_hash,wrong_data))
            entry['root']=wrong_hash.hex()
            changed+=1
    encoded=json.dumps(metadata,separators=(',',':')).encode()
    index.write_bytes(subprocess.check_output(['zstd','-q','-c'],input=encoded))
    assert json.loads(subprocess.check_output(['zstd','-q','-d','-c',str(index)]))==metadata
    index.unlink()
assert changed>0,'test did not tamper any cached tree'
root=pathlib.Path(os.environ['RETRY_ARCHIVE'])
before={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.glob('states/**/*.tree.age')}
(root.parent/'before-trees.json').write_text(json.dumps(before))
PY
printf 'third child\n' >> child
l save -qam third
ok 'retry rebuilds recovered cache trees through remote verification' l push archive -q
python3 - <<'PYRECHECK'
import hashlib,json,os,pathlib
root=pathlib.Path(os.environ['RETRY_ARCHIVE'])
for path,digest in json.loads((root.parent/'before-trees.json').read_text()).items():
    assert hashlib.sha256((root/path).read_bytes()).hexdigest()==digest,path
PYRECHECK
ok 'recovery ignores a structurally valid but incomplete cached tree' l fsck --remote archive --files
finish
