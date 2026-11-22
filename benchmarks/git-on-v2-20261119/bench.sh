#!/bin/bash
# git commands on one Chromium line: git store (base) against git on v2 through FUSE (v2).
# Needs: the v2 store of `store-v3-prototype history v2-cold chromium.git gv-work gv-scratch 1000`.
set -euo pipefail
S=${S:-$HOME/storev3}
B=${B:-$HOME/gitview/src/benchmarks/git-on-v2-20261119/target/release/git-on-v2}
ST=${ST:-$S/gv-scratch/store}
M=${M:-$S/gv}  # map name, without .map
W=$S/gv-work
MNT=$S/gv-mnt
OUT=${OUT:-$S/gv-results.jsonl}
unset GIT_INDEX_FILE
ulimit -n $(ulimit -Hn)
now() { date +%s%N; }

# 1. git ids of every file of the 1000 states.
if [ ! -f $M.map ]; then
  t=$(now); m=$($B map $ST 1000 $M.map); echo "{\"step\":\"map\",\"ms\":$(( ($(now)-t)/1000000 )),\"out\":$m}" >> $OUT
fi
git -C $S/chromium.git cat-file --batch-all-objects --batch-check='%(objecttype) %(objectname)' | awk '$1=="blob"{print $2}' | sort > $M.gitblobs
sort $M.shas > $M.mapblobs
echo "{\"step\":\"ids\",\"git_blobs\":$(wc -l < $M.gitblobs),\"map_blobs\":$(wc -l < $M.mapblobs),\"git_only\":$(comm -23 $M.gitblobs $M.mapblobs | wc -l),\"map_only\":$(comm -13 $M.gitblobs $M.mapblobs | wc -l)}" >> $OUT

LAST=$(git -C $S/chromium.git rev-list --first-parent -n 1 refs/heads/main)
# 2. Two git folders for the same work folder.
rm -rf $S/gd-base $S/gd-v2
for g in gd-base gd-v2; do
  git init -q --bare $S/$g
  git -C $S/$g config core.bare false
  cp $S/chromium.git/shallow $S/$g/shallow
done
echo $S/chromium.git/objects > $S/gd-base/objects/info/alternates
t=$(now)
git -C $S/chromium.git rev-list --objects --filter=blob:none refs/heads/main | git -C $S/chromium.git pack-objects -q --stdout | git -C $S/gd-v2 index-pack --stdin > /dev/null
echo "{\"step\":\"light pack\",\"ms\":$(( ($(now)-t)/1000000 )),\"bytes\":$(du -sb $S/gd-v2/objects/pack | cut -f1)}" >> $OUT
echo $MNT > $S/gd-v2/objects/info/alternates
# A blob whose git bytes differ from the file bytes (line ends): kept as a real git object, as a
# real build would keep it.
for x in $(comm -23 $M.gitblobs $M.mapblobs); do
  git -C $S/chromium.git cat-file blob $x | git --git-dir=$S/gd-v2 hash-object -w --stdin > /dev/null
done
for g in gd-base gd-v2; do
  git -C $S/$g update-ref refs/heads/main $LAST
  git -C $S/$g symbolic-ref HEAD refs/heads/main
done

mkdir -p $MNT
fusermount3 -u $MNT 2>/dev/null || true
$B mount $ST $M.map $MNT 2> $S/gv-mount.err &
for i in $(seq 100); do mountpoint -q $MNT && break; sleep 0.2; done
mountpoint -q $MNT

HOT=$(git --git-dir=$S/gd-base log --name-only --format= -n 999 | grep -v '^$' | sort | uniq -c | sort -rn | awk 'NR==1{print $2}')
HOTSRC=$(git --git-dir=$S/gd-base log --name-only --format= -n 999 -- '*.cc' | grep -v '^$' | sort | uniq -c | sort -rn | awk 'NR==1{print $2}')
echo "{\"step\":\"files\",\"hot\":\"$HOT\",\"hot_cc\":\"$HOTSRC\"}" >> $OUT

stats() { cat $M.stats 2>/dev/null || echo '{}'; }
run() { # name, command (runs with GIT_DIR and GIT_WORK_TREE set)
  local name=$1; shift
  for g in gd-base gd-v2; do
    for pass in 1 2; do
      mountpoint -q $MNT || { echo '{"step":"MOUNT GONE"}' >> $OUT; exit 1; }
      sleep 1.2; local s0; s0=$(stats)
      local t; t=$(now)
      local ok=true
      ( cd $W && GIT_DIR=$S/$g GIT_WORK_TREE=$W timeout 1200 bash -c "$*" > /dev/null ) 2> $S/gv-cmd.err || ok=false
      local ms=$(( ($(now)-t)/1000000 ))
      sleep 1.2
      echo "{\"step\":\"$name\",\"git\":\"$g\",\"pass\":$pass,\"ms\":$ms,\"ok\":$ok,\"load\":$(cut -d' ' -f1 /proc/loadavg),\"fuse_before\":$s0,\"fuse_after\":$(stats)}" >> $OUT
    done
  done
}
run "index" "git read-tree HEAD && git update-index -q --refresh"
run "fsck --connectivity-only" "git fsck --connectivity-only --no-dangling"
run "status" "git status --porcelain"
run "log 1000" "git log --oneline -n 1000"
run "log 1000 full ids" "git log --format=%H -n 1000"
run "log -p 100" "git log -p -n 100"
run "diff 100 commits" "git diff HEAD~100 HEAD"
run "log -p hot file" "git log -p -- '$HOT'"
run "blame hot .cc" "git blame -- '$HOTSRC'"
run "push 10 commits" "printf 'HEAD\n^HEAD~10\n' | git pack-objects --revs --stdout"
run "push 100 commits" "printf 'HEAD\n^HEAD~100\n' | git pack-objects --revs --stdout"
run "archive HEAD" "git archive HEAD"
run "push pack size, 100 commits" 'printf "HEAD\n^HEAD~100\n" | git pack-objects --revs --stdout | wc -c > '"$S"'/gv-push-$(basename $GIT_DIR).size'
fusermount3 -u $MNT
echo DONE >> $OUT
