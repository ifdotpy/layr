#!/bin/bash
# The files of the git history live once, in the object store (docs/git-on-objects.md): after
# `layr gc` the git store holds no blob that the object store has, and git in a line, a push and a
# new commit in a fast line still read every file. Linux with LayrFS (layrfs-fuse), or macOS with
# the LayrFS extension. Provide LAYR, an isolated LAYR_ROOT, and on Linux LAYR_SOCKET of its root
# daemon. Run as a non-root user.
set -u
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
[ "$(uname -s)" = Darwin ] || : "${LAYR_SOCKET:?}"
sum() { if command -v md5sum >/dev/null; then md5sum | cut -d' ' -f1; else md5 -q; fi; }
export LAYR_AUTHOR_NAME=Layr LAYR_AUTHOR_EMAIL=layr@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-git-objects.XXXXXX")" && pwd -P)
p=gitobj-$(date +%s)-$$
pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); echo "FAIL: $*" >&2; }
is() { if [ "$2" = "$3" ]; then ok; else bad "$1: got '$2', want '$3'"; fi; }
has() { case "$2" in *"$3"*) ok ;; *) bad "$1: '$2' has no '$3'" ;; esac; }

# The blobs in the git store's own packs and loose files (not the ones it reads through alternates).
local_blobs() {
  local s=$1 n=0 i
  for i in "$s"/objects/pack/*.idx; do
    [ -e "$i" ] || continue
    n=$((n + $(git verify-pack -v "$i" 2>/dev/null | awk '$2 == "blob"' | wc -l)))
  done
  echo $((n + $(find "$s"/objects -mindepth 2 -maxdepth 2 -path '*/objects/[0-9a-f][0-9a-f]/*' -type f | while read -r f; do
    o=$(basename "$(dirname "$f")")$(basename "$f"); [ "$(GIT_DIR=$s git cat-file -t "$o" 2>/dev/null)" = blob ] && echo; done | wc -l)))
}

# A history of several commits: changed, added and removed files, an executable, a link, a
# folder that goes, and a file larger than one zlib block.
src=$work/source; mkdir -p $src/d/e
( cd $src && git init -q -b main
  for i in 1 2 3 4 5 6; do
    printf 'line %s of commit %s\n' 1 $i > f1; printf 'commit %s\n' $i >> f$i.txt
    [ $i = 2 ] && { printf '#!/bin/sh\necho hi\n' > run.sh; chmod +x run.sh; ln -s f1 link; }
    [ $i = 3 ] && yes 'a line that repeats' | head -c 300000 > big
    [ $i = 4 ] && { printf 'x\n' > d/e/g; git rm -q f1.txt; }
    [ $i = 5 ] && git rm -rq d
    git add -A && git commit -q -m "commit $i"
  done )
head=$(git -C $src rev-parse HEAD)
want_log=$(git -C $src log -p --format='%H %s' | sum)
$L init $p --from-git $src -q >/dev/null 2>$work/init.err || { cat $work/init.err >&2; echo "init failed" >&2; exit 1; }
s=$LAYR_ROOT/projects/$p/git
cd $src

# After init the git store reads its blobs from the object store.
mp=$(cd $s/layr-v2 && pwd -P)
mount | grep -q " on $mp " && ok || bad "the folder of the blobs is not mounted"
has "alternates name it" "$(cat $s/objects/info/alternates 2>&1)" "$s/layr-v2"
is "no blob left in the git store" "$(local_blobs $s)" 0
ls $s/layr-v2/pack/*.pack >/dev/null 2>&1 && ok || bad "the folder shows no pack"

# git in the line reads every file of the history.
is "git log -p of the whole history" "$(git log -p --format='%H %s' $head | sum)" "$want_log"
is "git status" "$(git status --porcelain 2>&1)" ""
is "git fsck" "$(git fsck --connectivity-only --no-dangling 2>&1)" ""
is "a link" "$(git cat-file -p HEAD:link)" f1
is "an executable" "$(git ls-tree HEAD run.sh | cut -c1-6)" 100755
is "a large file" "$(git cat-file -s HEAD:big)" "$(wc -c < big | tr -d ' ')"

# A fast line: a new commit of git, its blob goes to the object store at the next gc.
$L line new fast --fast -q >/dev/null || { echo "line new --fast failed" >&2; exit 1; }
cd $LAYR_ROOT/projects/$p/lines/fast
printf 'new file of git\n' > n1
git add n1 && git commit -q -m "git: n1" || bad "git commit in the fast line"
is "layr follows git" "$($L show head:n1 2>&1)" "new file of git"
blob=$(git rev-parse HEAD:n1)
$L gc -q >/dev/null 2>$work/gc.err || { bad "gc"; cat $work/gc.err >&2; }
is "no blob left after the second gc" "$(local_blobs $s)" 0
is "git reads the new blob" "$(git cat-file -p $blob 2>&1)" "new file of git"
is "git log -p in the fast line" "$(git log -p --format='%H %s' $head | sum)" "$want_log"
printf 'second\n' >> n1
git commit -q -am "git: n1 again" || bad "a commit after gc"
is "git diff after gc" "$(git diff HEAD~1 --stat | tail -1 | tr -s ' ')" " 1 file changed, 1 insertion(+)"

# The folder does not mount: gc writes the blobs into the git store and git reads them there.
# When the folder mounts again, the next gc removes the copy.
if [ "$(uname -s)" = Darwin ]; then umount $mp; else fusermount3 -u $mp 2>/dev/null || fusermount -u $mp; fi
mount | grep -q " on $mp " && bad "the folder stays mounted" || ok
LAYR_TEST_GITPACK_NO_MOUNT=1 $L gc >$work/gc-copy.out 2>$work/gc-copy.err || { bad "gc without the folder"; cat $work/gc-copy.err >&2; }
has "gc says the git store holds the blobs" "$(cat $work/gc-copy.out $work/gc-copy.err)" "the git store holds the blobs"
mount | grep -q " on $mp " && bad "the folder mounts with LAYR_TEST_GITPACK_NO_MOUNT" || ok
[ "$(local_blobs $s)" -gt 0 ] && ok || bad "the git store holds no copy of the blobs"
is "git log -p without the folder" "$(git log -p --format='%H %s' $head | sum)" "$want_log"
is "git reads the new blob without the folder" "$(git cat-file -p $blob 2>&1)" "new file of git"
$L gc -q >/dev/null 2>$work/gc-back.err || { bad "gc with the folder again"; cat $work/gc-back.err >&2; }
mount | grep -q " on $mp " && ok || bad "the folder does not mount again"
is "gc removes the copy" "$(local_blobs $s)" 0
is "git log -p with the folder again" "$(git log -p --format='%H %s' $head | sum)" "$want_log"

# A push sends compressed objects that a clone reads.
git init -q --bare $work/remote.git
$L git remote add origin $work/remote.git >/dev/null 2>&1 || bad "git remote add"
$L git push origin fast >/dev/null 2>$work/push.err || { bad "git push"; cat $work/push.err >&2; }
is "the remote has the commit" "$(git -C $work/remote.git rev-parse refs/heads/fast 2>&1)" "$(git rev-parse HEAD)"
is "the remote is complete" "$(git -C $work/remote.git fsck --no-dangling 2>&1)" ""
git clone -q -b fast $work/remote.git $work/clone 2>/dev/null || bad "clone of the remote"
is "the clone has the files" "$(cat $work/clone/n1 2>&1)" "$(printf 'new file of git\nsecond')"
remote_kb=$(du -sk $work/remote.git/objects | cut -f1)
[ "$remote_kb" -lt 150 ] && ok || bad "the pushed objects are not compressed: $remote_kb KiB"

# The project goes: its git folder unmounts first, so nothing of it stays open.
cd $LAYR_ROOT/projects/$p
$L line delete -D -f fast main >/dev/null 2>&1 || bad "line delete"
cd $work
$L project delete $p >/dev/null 2>$work/delete.err && ok || { bad "project delete"; cat $work/delete.err >&2; }
mount | grep -q " on $mp " && bad "the git folder stays mounted" || ok

if [ $fail = 0 ]; then
  echo "PASS: git history in the object store ($pass checks)"
else
  echo "FAIL: $fail of $((pass + fail)) checks; evidence in $work" >&2; exit 1
fi
