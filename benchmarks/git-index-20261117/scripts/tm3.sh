#!/bin/bash
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; T=$HOME/gitview/tmp
now() { date +%s%N; }; ms() { echo $(( ($(now)-$1)/1000000 )); }
FILES=$(cd ~/gitview/kernel-src && git ls-files '*.c' | awk 'NR%2500==1' | head -10)
for r in 1 2 3; do
  n=tm3$r$$; cd ~/gitview/kernel-src; s=$(now); $L line new $n --fast -q || exit 1; ln=$(ms $s); cd $LINES/$n; g=$(readlink .git)
  for f in $FILES; do printf '/* t3 */\n' >> $f; done
  s=$(now); export GIT_INDEX_FILE=$T/tm3.idx; rm -f $GIT_INDEX_FILE*
  git read-tree HEAD && /usr/bin/time -f "%e s wall, user %U, sys %S" -o $T/tm3.time git add -A && tree=$(git write-tree); unset GIT_INDEX_FILE
  c=$(git commit-tree $tree -p HEAD -m cp) && git update-ref refs/t3code/cp $c; cp1=$(ms $s)
  s=$(now); export GIT_INDEX_FILE=$T/tm3.idx; rm -f $GIT_INDEX_FILE*; git read-tree HEAD && git add -A && tree=$(git write-tree); unset GIT_INDEX_FILE; cp2=$(ms $s)
  for f in $FILES; do printf '/* more */\n' >> $f; done
  s=$(now); git restore -s refs/t3code/cp -- . && git clean -fdq; rs=$(ms $s)
  printf "round %s: line new %5d ms, first checkpoint %6d ms (add -A %s), second %6d ms, restore %5d ms, loose objects %s, packs %s, load %s\n" $r $ln $cp1 "$(cat $T/tm3.time)" $cp2 $rs "$(find $g/objects -type f -path "*/objects/??/*" | wc -l)" "$(ls $g/objects/pack | wc -l)" "$(cut -d" " -f1 /proc/loadavg)"
  cd ~/gitview/kernel-src; $L line delete -D --discard $n -q >/dev/null 2>&1
done
