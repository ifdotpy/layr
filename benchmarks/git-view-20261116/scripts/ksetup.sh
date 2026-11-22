#!/bin/bash
# The kernel project of the git-view measurements: a copy of the kernel tree with a new git
# repository of one commit (the source is a shallow clone, which `init --from-git` does not take)
# becomes a layr project with object states.
set -u
eval "$(~/gitview/it.sh env)"
export LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=GV GIT_AUTHOR_EMAIL=gv@example.invalid GIT_COMMITTER_NAME=GV GIT_COMMITTER_EMAIL=gv@example.invalid
L=$LAYR
K=~/gitview/kernel-src
t() { local s=$(date +%s.%N); "$@"; local rc=$?; echo "TIME $(echo "$(date +%s.%N) - $s" | bc) s rc=$rc : $*"; }
[ -d $K ] || t cp -a --reflink=always /home/layrfs-src/linux $K
cd $K
if [ -f .git/shallow ]; then
  rm -rf .git
  git init -q -b main
  t git add -A
  t git commit -q -m "linux tree"
fi
git log --oneline | head -2
t $L init gvkernel --from-git $K
cd $K
t $L protect main --remove
t $L migrate --delete-old
$L log --oneline -n 3
$L status | head -3
echo SETUP-DONE
