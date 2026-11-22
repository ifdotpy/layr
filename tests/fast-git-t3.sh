#!/bin/bash
# The git commands of T3 Code in a fast line (docs/layrfs-lab.md): a checkpoint (read-tree and
# add -A into a temporary index, write-tree, commit-tree, update-ref refs/t3code/...), a diff,
# a restore (restore -s, clean -fd), and the page cache store that such a checkpoint starts in
# the view: every file still shows its own content, also one written while the store runs.
# Provide LAYR (on Linux built with layrfs-fuse), an isolated LAYR_ROOT, and on Linux
# LAYR_SOCKET of its root daemon. Run as a non-root user.
set -u
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
[ "$(uname -s)" = Darwin ] || : "${LAYR_SOCKET:?}"
export LAYR_AUTHOR_NAME=Layr LAYR_AUTHOR_EMAIL=layr@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-git-t3.XXXXXX")" && pwd -P)
p=fastt3-$(date +%s)-$$
pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); echo "FAIL: $*" >&2; }
is() { if [ "$2" = "$3" ]; then ok; else bad "$1: got '$2', want '$3'"; fi; }

# 600 files (a checkpoint reads them all, so the view stores them), a cache folder.
src=$work/src; mkdir -p $src
for d in a b c d e f; do mkdir -p $src/$d; for i in $(seq 100); do printf '%s %s\n' $d $i > $src/$d/f$i; done; done
printf 'target regenerable\n' > $src/.layrattributes; printf 'target/\n' > $src/.gitignore
# Older than the index of the new line by a second: git does not read them again (racy entries).
sleep 1.1
( cd $src && git init -q -b main && git add -A && git commit -q -m first )
$L init $p --from-git $src -q >/dev/null 2>&1 || { echo "init failed" >&2; exit 1; }
cd $src
$L protect main --remove >/dev/null
$L migrate --delete-old >/dev/null
$L line new f --fast -q || { echo "line new --fast failed" >&2; exit 1; }
w=$LAYR_ROOT/projects/$p/lines/f
cd $w
# The cache folder of the line (`.layrattributes`); no layr command reads the files after this.
mkdir -p target/debug; echo bin > target/debug/x
is "git status of a new line" "$(git status --porcelain 2>&1)" ""

checkpoint() {
  export GIT_INDEX_FILE=$work/t3.idx; rm -f $GIT_INDEX_FILE*
  git read-tree HEAD && git add -A && tree=$(git write-tree); unset GIT_INDEX_FILE
  c=$(git commit-tree $tree -p HEAD -m checkpoint) && git update-ref refs/t3code/$1 $c
}

# A checkpoint, while another process writes files: the writes stay.
printf 'edit\n' >> a/f1; printf 'new\n' > n1
( for i in $(seq 40); do printf 'during %s\n' $i >> b/f$i; done ) &
checkpoint cp1 || bad "checkpoint"
wait
sleep 2
if [ "$(uname -s)" != Darwin ] && [ -r "${LAYR_DAEMON_LOG:-}" ]; then
  # The files that the writer changed first are not stored: 560 to 600.
  stored=$(grep -o "stored [0-9]* files" "$LAYR_DAEMON_LOG" | tail -1 | cut -d' ' -f2)
  [ "${stored:-0}" -ge 550 ] && ok || bad "the view stored ${stored:-no} files, want at least 550"
fi
is "the checkpoint leaves the cache out" "$(git ls-tree -r --name-only refs/t3code/cp1 | grep -c '^target')" 0
is "file written during the store" "$(tail -1 b/f7)" "during 7"
is "file written during the store, in git" "$(git diff --name-only | grep -c '^b/')" 40
# Every file of the folder shows its content: the same as git's hash of it from outside the view.
bad_files=0
for f in $(git ls-files); do
  [ "$(git hash-object "$f")" = "$(git rev-parse ":$f" 2>/dev/null || echo none)" ] || {
    git diff --quiet -- "$f" 2>/dev/null && bad_files=$((bad_files + 1))
  }
done
is "files that differ from their own index entry" $bad_files 0
is "a/f1 after the checkpoint" "$(tail -1 a/f1)" edit
is "a clean index after the checkpoint" "$(git status --porcelain | grep -c '^ M')" 41

# The diff and the restore of T3 Code.
is "diff against the checkpoint" "$(git diff --name-status HEAD refs/t3code/cp1 | sort | head -2 | tr '\t\n' ' ')" "A n1 M a/f1 "
printf 'later\n' >> a/f1; echo junk > junk
git restore -s refs/t3code/cp1 -- . && git clean -fdq || bad "restore"
is "a/f1 after restore" "$(tail -1 a/f1)" edit
[ -e junk ] && bad "junk stays after clean" || ok
is "cache after clean" "$(cat target/debug/x 2>&1)" bin
is "layr and git agree" "$($L status --porcelain | sort | md5sum)" "$(git status --porcelain | sort | md5sum)"

cd $src
$L line delete -D --discard f -q >/dev/null 2>&1
if [ $fail = 0 ]; then
  echo "PASS: git commands of T3 Code in a fast line ($pass checks)"
else
  echo "FAIL: $fail of $((pass + fail)) checks; evidence in $work" >&2; exit 1
fi
