#!/bin/bash
# Old (0d21b45: whole index, layr copies its index) against new (split index, git resets the index).
# Each round: the daemon of one binary, a new fast line, the steps, the line goes; then the other.
set -u
export GIT_CONFIG_GLOBAL=/dev/null LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
R=$HOME/gitview/ab.jsonl; rounds=${1:-5}
now() { date +%s%N; }
for round in $(seq $rounds); do
  for v in old new; do
    [ $v = old ] && bin=$HOME/gitview/src-old/target/release/layr || bin=$HOME/gitview/src/target/release/layr
    ~/gitview/it.sh down >/dev/null 2>&1; BIN=$bin ~/gitview/it.sh up >/dev/null
    eval "$(~/gitview/it.sh env)"; L=$LAYR; LINES=$LAYR_ROOT/projects/gvkernel/lines; n=ab$v$round$$
    cd ~/gitview/kernel-src
    m() { local step=$1; shift; local s=$(now); "$@" >/dev/null 2>/tmp/ab.err; local rc=$?
      local ms=$(( ($(now)-s)/1000000 )); [ $rc = 0 ] || { step="$step FAILED"; head -2 /tmp/ab.err; }
      echo "{\"v\":\"$v\",\"step\":\"$step\",\"ms\":$ms,\"round\":$round,\"load\":$(cut -d' ' -f1 /proc/loadavg)}" >> $R; }
    wait_head() { local s=$(now); for i in $(seq 200); do [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] && break; sleep 0.05; done; echo $(( ($(now)-s)/1000000 )); }
    m "line new" $L line new $n --fast -q
    cd $LINES/$n
    FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
    edit() { for f in $FILES; do printf '/* %s */\n' "$1" >> $f; done; }
    m "git status (first)" git status --porcelain
    m "git status (warm)" git status --porcelain
    m "git status (warm 2)" git status --porcelain
    edit a; m "git status after 10 edits" git status --porcelain
    m "git diff --stat" git diff --stat
    m "git commit -a (10 files)" git commit -q -am "a $round"
    echo "{\"v\":\"$v\",\"step\":\"head follows git commit\",\"ms\":$(wait_head),\"round\":$round}" >> $R
    m "git status after git commit" git status --porcelain
    m "git reset --hard HEAD~1" git reset -q --hard HEAD~1
    echo "{\"v\":\"$v\",\"step\":\"head follows git reset\",\"ms\":$(wait_head),\"round\":$round}" >> $R
    m "git status after git reset" git status --porcelain
    edit b; m "layr save -a (10 files)" $L save -a -q -m "b $round"
    m "git status after layr save" git status --porcelain
    m "git status after layr save 2" git status --porcelain
    m "layr undo" $L undo
    m "git status after layr undo" git status --porcelain
    [ -z "$(git status --porcelain)" ] || echo "$v $round: git status not clean" >&2
    [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head)" ] || echo "$v $round: heads differ" >&2
    cd ~/gitview/kernel-src; $L line delete -D $n -q >/dev/null 2>&1 || echo "$v $round: line delete failed" >&2
  done
done
~/gitview/it.sh down >/dev/null 2>&1
python3 - $R <<'P'
import json,sys,statistics as st
rows=[json.loads(l) for l in open(sys.argv[1])]
print(f'{"step":30}{"old":>8}{"new":>8}')
for s in dict.fromkeys(r["step"] for r in rows):
  v={k:st.median([r["ms"] for r in rows if r["step"]==s and r["v"]==k] or [0]) for k in ("old","new")}
  print(f'{s:30}{v["old"]:>8.0f}{v["new"]:>8.0f}')
print("load", st.median(r["load"] for r in rows if "load" in r))
P
