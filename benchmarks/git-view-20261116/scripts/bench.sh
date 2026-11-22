#!/bin/bash
# Git in a fast line against git in a classic line and git in a plain folder (kukuka-wsl).
# usage: bench.sh <rounds>   (after it.sh up and ksetup.sh; results: ~/gitview/results.jsonl)
set -u
eval "$(~/gitview/it.sh env)"
export LAYR_AUTHOR_NAME=GV LAYR_AUTHOR_EMAIL=gv@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR
P=${PROJECT:-gvkernel}
K=${SOURCE:-$HOME/gitview/kernel-src}          # the main line of the project
N=${PLAIN:-$HOME/gitview/kernel-plain}         # a plain copy with its own .git
R=${RESULTS:-$HOME/gitview/results.jsonl}
rounds=${1:-3}
now() { date +%s%N; }
rec() { echo "{\"kind\":\"$1\",\"step\":\"$2\",\"ms\":$3,\"round\":$round,\"load\":$(cut -d' ' -f1 /proc/loadavg)}" >> "$R"; printf '%-8s %-34s %8s ms\n' "$1" "$2" "$3"; }
# m <kind> <step> <command>...: run once, record the time; a failure is recorded as a step "<step> FAILED".
m() {
  local kind=$1 step=$2; shift 2
  local s=$(now); "$@" >/dev/null 2>$HOME/gitview/tmp/bench.err; local rc=$?
  local ms=$(( ($(now) - s) / 1000000 ))
  [ $rc = 0 ] && rec "$kind" "$step" $ms || { rec "$kind" "$step FAILED rc=$rc" $ms; head -3 $HOME/gitview/tmp/bench.err >&2; }
}
cd $K
FILES=$(git ls-files '*.c' | awk 'NR%2500==1' | head -10)
edit() { for f in $FILES; do printf '/* bench %s */\n' "$1" >> $f; done; }
# The plain folder: the same files with a git repository of its own (one commit).
[ -d $N/.git/refs ] && [ -w $N/.git ] || {
  rm -rf $N.tmp; cp -a --reflink=always $K $N.tmp; chmod -R u+w $N.tmp/.git; rm -rf $N.tmp/.git $N
  ( cd $N.tmp && git init -q -b main && git add -A && git commit -q -m "linux tree" ) && mv $N.tmp $N
  ( cd $N && git status >/dev/null )
}

for round in $(seq $rounds); do
  # --- a plain folder: git only -------------------------------------------------------------
  cd $N
  m plain "git status (warm)" git status --porcelain
  edit "plain $round a"
  m plain "git status after 10 edits" git status --porcelain
  m plain "git diff --stat" git diff --stat
  m plain "git commit -a (10 files)" git commit -q -am "bench plain $round"
  m plain "git reset --hard HEAD~1" git reset -q --hard HEAD~1

  for kind in classic fast; do
    # A classic line is slow in a tree of this size: fewer rounds.
    [ $kind = classic ] && [ $round -gt ${CLASSIC_ROUNDS:-3} ] && continue
    n=$kind$round-$$
    cd $K
    m $kind "line new" $L line new $n --$kind -q
    cd $LAYR_ROOT/projects/$P/lines/$n || continue
    m $kind "git status (first)" git status --porcelain
    m $kind "git status (warm)" git status --porcelain
    m $kind "git status (warm 2)" git status --porcelain
    m $kind "layr status (first)" $L status --porcelain
    edit "$kind $round a"
    m $kind "git status after 10 edits" git status --porcelain
    m $kind "git diff --stat" git diff --stat
    if [ $kind = fast ]; then
      # git commit: the hook moves the head of the line.
      m $kind "git commit -a (10 files)" git commit -q -am "bench git $round"
      [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] || rec $kind "the head did not follow FAILED" 0
      m $kind "layr status after git commit" $L status --porcelain
      m $kind "git status after git commit" git status --porcelain
      # The two parts: git without the hook, then the head follows.
      edit "$kind $round b"
      m $kind "git commit -a, no hook" git -c core.hooksPath=/dev/null commit -q -am "bench git $round b"
      m $kind "layr git adopt" $L git adopt -q
      m $kind "git reset --hard HEAD~1" git reset -q --hard HEAD~1
      [ "$(git rev-parse HEAD)" = "$($L rev-parse --git head 2>/dev/null)" ] || rec $kind "the head did not follow a reset FAILED" 0
    fi
    edit "$kind $round c"
    m $kind "layr save -a (10 files)" $L save -a -q -m "bench layr $round"
    m $kind "git status after layr save" git status --porcelain
    m $kind "git log -n 5" git log -n 5 --format=%s
    m $kind "layr reset --hard head~1" $L reset --hard -q head~1
    m $kind "git status after layr reset" git status --porcelain
    if [ $kind = fast ]; then
      m $kind "git status, no fsmonitor" git -c core.fsmonitor=false -c core.untrackedCache=false status --porcelain
    fi
    cd $K
    m $kind "line delete" $L line delete -D $n
  done
done
