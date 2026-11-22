#!/bin/bash
# E2: a line whose store has no blobs; reads copy each blob from a remote store at first use.
. ~/lfx/lab/lab.sh
LO=/home/layr/lab/lazy-objects
sudo -u layr rm -rf $LO; sudo -u layr mkdir -p $LO/blobs $LO/tmp
sudo -u layr cp -a --reflink=auto $O/config.json $O/dicts $O/objects $O/packs $O/states $LO/
rec e2 "store size: full (KiB)" "$(sudo du -sk $O | cut -f1)"
rec e2 "store size: without blobs (KiB)" "$(sudo du -sk $LO | cut -f1)"
fetched() { vctl $1 stats | grep -o '"fetched":{[^}]*}'; }
for tree in linux layr; do
  n=lz-$tree
  t0=$(now); OBJ=$LO vnew $n $tree-base --remote $O; t1=$(now); rec e2 "$tree lazy: line new" $(ms $t0 $t1)
  P=$(M $n)
  until vctl $n stats | grep -q '"index_entries":[0-9]'; do sleep 0.02; done
  rec e2 "$tree lazy: after index" 0 "$(fetched $n)"
  G=(git -c "core.fsmonitor=$L fs view fsmonitor" -c core.untrackedCache=true -C "$P")
  t0=$(now); "${G[@]}" status --porcelain >/dev/null; t1=$(now); rec e2 "$tree lazy: git status (cold)" $(ms $t0 $t1) "$(fetched $n)"
  if [ $tree = linux ]; then
    t0=$(now); (cd $P && rg -l TODO kernel/sched </dev/null >/dev/null || true); t1=$(now); rec e2 "linux lazy: rg kernel/sched (first)" $(ms $t0 $t1) "$(fetched $n)"
    t0=$(now); (cd $P && rg -l TODO </dev/null >/dev/null || true); t1=$(now); rec e2 "linux lazy: rg whole tree (first)" $(ms $t0 $t1) "$(fetched $n)"
  else
    t0=$(now); (cd $P && cargo build --offline -q 2>/dev/null); t1=$(now); rec e2 "layr lazy: cargo build (full)" $(ms $t0 $t1) "$(fetched $n)"
  fi
  vdel $n
done
sudo -u layr rm -rf $LO
