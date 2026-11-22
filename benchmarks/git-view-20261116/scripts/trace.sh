#!/bin/bash
set -u
eval "$(~/gitview/it.sh env)"
export LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR
K=$HOME/gitview/kernel-src
cd $K
n=tr$$
$L line new $n --fast -q
cd $LAYR_ROOT/projects/gvkernel/lines/$n
tr() { echo "=== $*"; LAYR_TRACE=1 "$@" 2>&1 | grep "^trace" | awk '$2+0 >= 30' | tail -25; }
tr $L status --porcelain
tr $L status --porcelain
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
for f in $FILES; do echo '/* t */' >> $f; done
git -c core.hooksPath=/dev/null commit -q -am "trace commit"
tr $L git adopt
tr $L status --porcelain
for f in $FILES; do echo '/* t2 */' >> $f; done
tr $L save -a -q -m "trace save"
cd $K; $L line delete -D $n >/dev/null 2>&1
