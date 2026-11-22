#!/bin/bash
# The cost of .git inside LayrFS: the same fast line, git with its folder outside (native, now)
# and with a copy of it inside the mount. Hooks off in both, so only git's own work counts.
set -u
eval "$(~/gitview/it.sh env)"
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; R=$HOME/gitview/gitin.jsonl
rounds=${1:-5}
now() { date +%s%N; }
cd ~/gitview/kernel-src
$L line new gin --fast -q || exit 1
w=$LINES/gin; cd $w
out=$(readlink .git)
cp -a $out $w/.gin; echo /.gin >> $w/.gin/info/exclude; echo /.gin >> $out/info/exclude
du -sh $out | cut -f1 | sed 's/^/size of .git: /'
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
m() { local kind=$1 step=$2; shift 2; local s=$(now); "$@" >/dev/null 2>/tmp/gin.err; local rc=$?
  local ms=$(( ($(now)-s)/1000000 )); [ $rc = 0 ] || { step="$step FAILED"; head -2 /tmp/gin.err; }
  echo "{\"kind\":\"$kind\",\"step\":\"$step\",\"ms\":$ms,\"round\":$round,\"load\":$(cut -d' ' -f1 /proc/loadavg)}" >> $R; }
for round in $(seq $rounds); do
  for kind in outside inside; do
    if [ $kind = outside ]; then G() { git -c core.hooksPath=/dev/null "$@"; }
    else G() { git --git-dir=$w/.gin --work-tree=$w -c core.hooksPath=/dev/null "$@"; }; fi
    m $kind "git status (warm)" G status --porcelain
    m $kind "git status (warm 2)" G status --porcelain
    for f in $FILES; do printf '/* %s %s */\n' $kind $round >> $f; done
    m $kind "git status after 10 edits" G status --porcelain
    m $kind "git diff --stat" G diff --stat
    m $kind "git add -u" G add -u
    m $kind "git commit (10 files)" G commit -q -m "$kind $round"
    m $kind "git log -n 20" G log -n 20 --oneline; m $kind "git show --stat HEAD" G show --stat HEAD
    cob() { G checkout -q -b t$kind$round && G checkout -q -; }; m $kind "git checkout -b + back" cob
    m $kind "git branch -D" G branch -q -D t$kind$round
    m $kind "git reset --hard HEAD~1" G reset -q --hard HEAD~1
    m $kind "git gc --auto" G gc --auto -q
  done
done
cd ~/gitview/kernel-src; $L line delete -D gin -q >/dev/null 2>&1
python3 - $R <<'P'
import json,sys,statistics as st
rows=[json.loads(l) for l in open(sys.argv[1])]
steps=list(dict.fromkeys(r["step"] for r in rows))
print(f'{"step":30} {"outside":>9} {"inside":>9}')
for s in steps:
  v={k:st.median([r["ms"] for r in rows if r["step"]==s and r["kind"]==k] or [0]) for k in ("outside","inside")}
  print(f'{s:30} {v["outside"]:>9.0f} {v["inside"]:>9.0f}')
print("load", st.median(r["load"] for r in rows))
P
