#!/bin/bash
# .git outside and inside LayrFS, with git settings that make the index write smaller.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; R=$HOME/gitview/gitin3.jsonl
rounds=${1:-5}; n=gin$$
now() { date +%s%N; }
cd ~/gitview/kernel-src
$L line new $n --fast -q || exit 1
w=$LINES/$n; cd $w; out=$(readlink .git); cdir=$(dirname $out)
VARIANTS="base split splitobj"
setv() { # setv <git dir> <variant>
  local g=$1
  case $2 in
    split|splitobj) git --git-dir=$g config core.splitIndex true ;;
    v4) git --git-dir=$g config index.version 4 ;;
    skiphash) git --git-dir=$g config index.skipHash true ;;
    all) git --git-dir=$g config core.splitIndex true; git --git-dir=$g config index.version 4; git --git-dir=$g config index.skipHash true ;;
  esac
  git --git-dir=$g --work-tree=$w -c core.hooksPath=/dev/null update-index --force-write-index $( [ $2 != base ] && echo --split-index)
  git --git-dir=$g --work-tree=$w status --porcelain >/dev/null
}
for v in $VARIANTS; do
  for kind in outside inside; do
    [ $kind = outside ] && g=$cdir/.g-$v || g=$w/.g-$v
    cp -a $out $g; echo "/.g-*" >> $g/info/exclude
    if [ $v = splitobj ]; then mkdir -p $cdir/obj-$kind; cp -a $g/objects/. $cdir/obj-$kind/; rm -rf $g/objects; ln -s $cdir/obj-$kind $g/objects; fi
    setv $g $v
  done
done
echo "/.g-*" >> $out/info/exclude
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
m() { local kind=$1 step=$2; shift 2; local s=$(now); "$@" >/dev/null 2>/tmp/gin2.err; local rc=$?
  local ms=$(( ($(now)-s)/1000000 )); [ $rc = 0 ] || { step="$step FAILED"; head -2 /tmp/gin2.err; }
  echo "{\"v\":\"$v\",\"kind\":\"$kind\",\"step\":\"$step\",\"ms\":$ms,\"round\":$round}" >> $R; }
for round in $(seq $rounds); do
  for v in $VARIANTS; do
    for kind in outside inside; do
      [ $kind = outside ] && g=$cdir/.g-$v || g=$w/.g-$v
      G() { git --git-dir=$g --work-tree=$w -c core.hooksPath=/dev/null "$@"; }
      G reset -q --hard HEAD
      m $kind "git status (warm)" G status --porcelain
      m $kind "git status (warm 2)" G status --porcelain
      for f in $FILES; do printf '/* %s %s %s */\n' $v $kind $round >> $f; done
      m $kind "git status after 10 edits" G status --porcelain
      m $kind "git add -u" G add -u
      m $kind "git commit (10 files)" G commit -q -m "$v $kind $round"
      m $kind "git reset --hard HEAD~1" G reset -q --hard HEAD~1
    done
  done
done
ls -la $w/.g-all | grep -i "index"
cd ~/gitview/kernel-src; $L line delete -D $n -q >/dev/null 2>&1
python3 - $R <<'P'
import json,sys,statistics as st
rows=[json.loads(l) for l in open(sys.argv[1])]
steps=list(dict.fromkeys(r["step"] for r in rows)); vs=list(dict.fromkeys(r["v"] for r in rows))
print(f'{"step":28}'+"".join(f'{v+" out":>11}{v+" in":>10}' for v in vs))
for s in steps:
  line=f'{s:28}'
  for v in vs:
    for k in ("outside","inside"):
      x=[r["ms"] for r in rows if r["step"]==s and r["kind"]==k and r["v"]==v]
      line+=f'{(st.median(x) if x else 0):>{11 if k=="outside" else 10}.0f}'
  print(line)
P
echo load $(cut -d' ' -f1 /proc/loadavg)
