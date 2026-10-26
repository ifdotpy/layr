#!/bin/bash
# Recover a lost push index while the complete per-state cache files remain.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init retry --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/retry/lines/main"
l line new topic -q
cd ../topic
printf 'first child\n' > child
l save -qam first
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q
export RETRY_CACHE=$LAYR_ROOT/projects/retry/meta/remote-stores RETRY_ARCHIVE=$T/archive
python3 - <<'PY'
import hashlib,json,os,pathlib
cache=pathlib.Path(os.environ['RETRY_CACHE'])
indexes=list(cache.glob('*/pushed.json'))
assert len(indexes)==1,indexes
index=json.loads(indexes[0].read_text())
assert index['states']
for state,entry in index['states'].items():
    path=indexes[0].parent/'pushed'/f"{state}.{entry['attempt']}.json"
    tree=json.loads(path.read_text())
    assert tree['depth']==entry['depth']
indexes[0].unlink()
root=pathlib.Path(os.environ['RETRY_ARCHIVE'])
before={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.glob('states/**/*.tree.age')}
assert before
(root.parent/'before-trees.json').write_text(json.dumps(before))
PY
printf 'second child\n' >> child
l save -qam second
ok 'push resumes with complete state caches but no index checkpoint' l push archive -q
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
finish
