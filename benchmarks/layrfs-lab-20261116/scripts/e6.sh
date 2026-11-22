#!/bin/bash
# E6: line new with 20 lines alive: one server process for all lines (hub) against one process per
# line and Btrfs snapshots.
. ~/lfx/lab/lab.sh
N=${N:-20}
H=/home/layr/run/hub.sock
t0=$(now); sudo -u layr true; t1=$(now); rec e6 "sudo -u layr true" $(ms $t0 $t1)
opts() { echo --objects $O --state $1-base --line /home/layr/store/lines/$2 --mnt /home/layr/lines/$2 \
       --sock /home/layr/run/$2.sock --cache target=$B/caches/$2/target --allow-other --allow $me --owner $me; }
series() { # series <tree> <label> <new command...>: N lines kept alive, then deleted
  local tree=$1 label=$2 c=() d=() i n; shift 2
  for i in $(seq $N); do
    n=$label-$tree-$i; mkdir -p $B/caches/$n/target
    t0=$(now); "$@" $(opts $tree $n) >/dev/null; t1=$(now); c+=($(ms $t0 $t1))
    [ -e /home/layr/lines/$n/Makefile ] || [ -e /home/layr/lines/$n/Cargo.toml ] || { echo "line $n has no files"; exit 1; }
  done
  local rss=$(ps -o rss= -p "$(pgrep -f 'fs view hub' | head -1)" 2>/dev/null || echo 0)
  local all=$(ps -o rss= -u layr | awk '{s+=$1} END {print s}')
  rec e6 "$tree $label: line new, $N alive (median)" "$(printf '%s\n' "${c[@]}" | median)" "\"all\":\"${c[*]}\",\"rss_kb_layr_user\":$all"
  for i in $(seq $N); do
    n=$label-$tree-$i
    t0=$(now); vctl $n stop remove >/dev/null; t1=$(now); d+=($(ms $t0 $t1)); rm -rf $B/caches/$n
  done
  rec e6 "$tree $label: line delete (median)" "$(printf '%s\n' "${d[@]}" | median)" "\"all\":\"${d[*]}\""
}
hub_up() { sudo -u layr sh -c "setsid $L fs view hub --sock $H --allow $me >/home/layr/run/hub.out 2>>/home/layr/run/hub.log </dev/null &"; until [ -S $H ]; do sleep 0.01; done; }
hub_down() { $L fs view ctl $H stop >/dev/null; while [ -S $H ]; do sleep 0.01; done; sleep 0.5; }
sudo -u layr rm -f /home/layr/run/hub.log
for tree in linux layr; do
  hub_up; series $tree hub-uring $L fs view ctl $H new --no-open --uring; hub_down
  hub_up; series $tree hub-classic $L fs view ctl $H new --no-open; hub_down
  series $tree fork-uring sudo -u layr $L fs view start --no-open --uring
  c=(); d=()
  for i in $(seq $N); do t0=$(now); sudo btrfs subvolume snapshot $B/btrfs/$tree-base $B/btrfs/lines/e6-$i >/dev/null; t1=$(now); c+=($(ms $t0 $t1)); done
  rec e6 "$tree btrfs: line new, $N alive (median)" "$(printf '%s\n' "${c[@]}" | median)" "\"all\":\"${c[*]}\""
  for i in $(seq $N); do t0=$(now); sudo btrfs subvolume delete $B/btrfs/lines/e6-$i >/dev/null; t1=$(now); d+=($(ms $t0 $t1)); done
  rec e6 "$tree btrfs: line delete (median)" "$(printf '%s\n' "${d[@]}" | median)"
done
sudo -u layr cat /home/layr/run/hub.log | grep "line ready" | awk '{print $(NF-1)*1000}' | median | { read m; rec e6 "hub: in-process line new (median, all)" "$m"; }
