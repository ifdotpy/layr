#!/bin/bash
# Exact Git DAG and object bytes survive encrypted store push and a fresh-machine clone.
. "$(dirname "$0")/lib.sh"
set -e
S=$T/source
mkdir -p "$S"
git -C "$S" init -q -b main
cd "$S"
printf 'one\n' > text
printf '\000\377binary\n' > binary
printf 'tracked-ignored\n' > ignored.log
printf '*.log\n' > .gitignore
printf '#!/bin/sh\nexit 0\n' > executable
chmod +x executable
ln -s text link
git add -f .
git update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,module
git commit -qm first
printf 'two\n' >> text
git commit -qam second
git switch -qc side
mkdir -p nested
printf 'shared\n' > nested/shared
git add nested
git commit -qm side
# Preserve arbitrary headers, distinct identities, timezones, signatures and non-UTF-8 messages.
python3 - <<'PY'
import subprocess
raw = subprocess.check_output(['git', 'cat-file', 'commit', 'HEAD'])
head, message = raw.split(b'\n\n', 1)
raw = head + b'\nencoding ISO-8859-1\ngpgsig -----BEGIN PGP SIGNATURE-----\n example\n -----END PGP SIGNATURE-----\n\nside caf\xe9\n'
oid = subprocess.check_output(['git', 'hash-object', '-w', '-t', 'commit', '--stdin'], input=raw).strip()
subprocess.run(['git', 'update-ref', 'refs/heads/side', oid], check=True)
PY
git switch -q main
mkdir -p nested
printf 'shared\n' > nested/shared
git add nested
git commit -qm third
git merge --no-ff side -qm merge
original_tip=$(git rev-parse main)
original_ids=$(git rev-list main | sort)
original_tree=$(git rev-parse 'main^{tree}')

eq 'fixture contains five commits' 5 "$(git rev-list --count main)"
l git clone "$S" history -q >/dev/null
cd "$LAYR_ROOT/projects/history/lines/main"
eq 'import maps all five commits' 5 "$(l log --format=%s | wc -l | tr -d ' ')"
eq 'import keeps original Git tip' "$original_tip" "$(git rev-parse HEAD)"
records=$(l op log --all | wc -l)
l git fetch -q
eq 'reimport creates no duplicate states' "$records" "$(l op log --all | wc -l)"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q
original_root=$LAYR_ROOT
new_fs "$T/replica" 8G
export LAYR_ROOT=$T/replica LAYR_PAPER_KEY_FILE=$T/paper
cd "$T"
l clone "file://$T/archive" history -q
cd "$LAYR_ROOT/projects/history/lines/main"
eq 'fresh clone restores identical commit IDs' "$original_ids" "$(git rev-list HEAD | sort)"
eq 'fresh clone restores exact original tree' "$original_tree" "$(git rev-parse 'HEAD^{tree}')"
eq 'tracked ignored file survives' tracked-ignored "$(cat ignored.log)"
eq 'symlink survives' text "$(readlink link)"
ok 'executable survives' test -x executable
ok 'restored Git objects are connected' git fsck --no-reflogs
R=$T/mirror.git
git init --bare -q -b main "$R"
l git remote add github "$R"
l git push -q github main
eq 'empty mirror receives all original IDs' "$original_ids" "$(git -C "$R" rev-list main | sort)"
eq 'mirror tip is original commit' "$original_tip" "$(git -C "$R" rev-parse main)"
printf 'new\n' > after
l add after
l save -qm after
l git push -q github main
new_tip=$(git -C "$R" rev-parse main)
eq 'new commit continues original history' "$original_tip" "$(git -C "$R" rev-parse main^)"
eq 'new commit adds one to count' 6 "$(git -C "$R" rev-list --count main)"
l reset --hard -q head~1
out=$(l git push github main 2>&1) && rc=0 || rc=$?
eq 'non-fast-forward returns failure' 128 "$rc"
has 'refusal names remote ID' "$new_tip" "$out"
has 'refusal names proposed ID' "$original_tip" "$out"
eq 'refusal keeps mirror unchanged' "$new_tip" "$(git -C "$R" rev-parse main)"
l git push --force -q github main
eq 'explicit force permits rewind' "$original_tip" "$(git -C "$R" rev-parse main)"
# Kill before the first 128-state index checkpoint, then recover from immutable remote trees.
K=$T/retry-source
mkdir -p "$K"
git -C "$K" init -q -b main
for n in $(seq 1 64); do
    printf 'version %s\n' "$n" > "$K/payload"
    git -C "$K" add payload
    git -C "$K" commit -qm "version $n"
done
l git clone "$K" retry -q >/dev/null
cd "$LAYR_ROOT/projects/retry/lines/main"
l remote add archive "file://$T/retry-archive" --paper-key-out "$T/retry-paper" >/dev/null
export RETRY_ARCHIVE=$T/retry-archive RETRY_BINARY=$LAYR RETRY_LOG=$T/retry-push.log
python3 - <<'PYCODE'
import hashlib,os,pathlib,subprocess,time
root=pathlib.Path(os.environ['RETRY_ARCHIVE'])
with open(os.environ['RETRY_LOG'],'wb') as log:
 p=subprocess.Popen([os.environ['RETRY_BINARY'],'push','archive','-q'],stdout=log,stderr=log)
 deadline=time.monotonic()+60
 while p.poll() is None and time.monotonic()<deadline:
  complete=list(root.glob('states/**/*.tree.age'))
  if len(complete)>=4:
   p.kill();p.wait();break
  time.sleep(.002)
 else:
  p.kill();p.wait()
  raise AssertionError('push did not reach the interruption point')
complete=list(root.glob('states/**/*.tree.age'))
assert 4<=len(complete)<64,len(complete)
before={str(f.relative_to(root)):hashlib.sha256(f.read_bytes()).hexdigest() for f in root.rglob('*') if f.is_file()}
subprocess.run([os.environ['RETRY_BINARY'],'push','archive','-q'],check=True)
for path,digest in before.items():
 assert hashlib.sha256((root/path).read_bytes()).hexdigest()==digest,path
complete=list(root.glob('states/**/*.tree.age'))
assert len(complete)==64,len(complete)
assert len({f.parent.name for f in complete})==64,'a retry duplicated a committed state'
print('PASS killed push resumes without replacing or duplicating committed objects')
PYCODE
PASS=$((PASS+1))
ok 'resumed store has complete records and verified file objects' l fsck --remote archive --files
# Incremental restore handles deletions and file/directory/symlink transitions.
rm payload
mkdir payload
printf 'nested\n' > payload/nested
ln -s payload/nested switched
l add -A
l save -qm 'directory and symlink'
l push archive -q
rm -r payload
printf '#!/bin/sh\nexit 0\n' > payload
chmod +x payload
rm switched
printf 'regular\n' > switched
l add -A
l save -qm 'executable and regular file'
l push archive -q
cd "$T"
l clone "file://$T/retry-archive" retry-copy -q
cd "$LAYR_ROOT/projects/retry-copy/lines/main"
eq 'parent snapshot restores a directory replaced by a file' '#!/bin/sh' "$(head -1 payload)"
ok 'parent snapshot restores executable mode' test -x payload
eq 'parent snapshot replaces a symlink with regular bytes' regular "$(cat switched)"
eq 'intermediate directory state remains readable' nested "$(l show head~1:payload/nested)"
finish
