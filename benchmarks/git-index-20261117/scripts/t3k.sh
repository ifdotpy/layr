#!/bin/bash
# T3 checkpoint on the kernel tree: plain git against a fast line.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=t3k$$; T=$HOME/gitview/tmp
now() { date +%s%N; }
t() { local name=$1; shift; local s=$(now); "$@" >/dev/null 2>/tmp/t3k.err; local rc=$?; printf '%-8s %-30s %7d ms rc=%s\n' $kind "$name" $(( ($(now)-s)/1000000 )) $rc; [ $rc = 0 ] || head -2 /tmp/t3k.err; }
cp_make() { export GIT_INDEX_FILE=$T/t3idx.$kind; rm -f $GIT_INDEX_FILE; git read-tree HEAD && git add -A && tree=$(git write-tree) && unset GIT_INDEX_FILE && c=$(git commit-tree $tree -p HEAD -m cp) && git update-ref refs/t3code/cp $c; unset GIT_INDEX_FILE; }
cp_make_kept() { export GIT_INDEX_FILE=$T/t3idx.$kind; git add -A && tree=$(git write-tree) && unset GIT_INDEX_FILE && c=$(git commit-tree $tree -p HEAD -m cp) && git update-ref refs/t3code/cp $c; unset GIT_INDEX_FILE; }
cp_restore() { git restore -s refs/t3code/cp -- . && git clean -fdq; }
FILES=$(cd ~/gitview/kernel-src && git ls-files '*.c' | awk 'NR%2500==1' | head -10)
cd ~/gitview/kernel-src; $L line new $n --fast -q || exit 1
for kind in plain fast; do
  [ $kind = plain ] && cd $HOME/gitview/kernel-plain || cd $LINES/$n
  git status --porcelain >/dev/null
  for i in 1 2; do
    for f in $FILES; do printf '/* t3 %s */\n' $i >> $f; done
    t "checkpoint (read-tree, add -A)" cp_make
    t "checkpoint (kept temp index)" cp_make_kept
    for f in $FILES; do printf '/* more */\n' >> $f; done
    t "restore (restore -s, clean -fd)" cp_restore
    t "git diff --numstat HEAD cp" git diff --numstat HEAD refs/t3code/cp
  done
  git checkout -q -- . ; git update-ref -d refs/t3code/cp
done
cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
echo load $(cut -d" " -f1 /proc/loadavg)
