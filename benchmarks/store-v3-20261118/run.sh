#!/bin/bash
# usage: run.sh <trials> <rounds> <batch>  (kukuka-wsl; corpus: the Linux kernel without .git)
set -u
B=~/gitview/src/benchmarks/store-v3-20261118/target/release/store-v3-prototype
W=~/storev3; C=$W/corpus; OUT=$W/results.jsonl
[ -d $C ] || { cp -a --reflink=always /home/layrfs-src/linux $C && rm -rf $C/.git; }
engines=(v3 v2-hot v2-cold)
for t in $(seq 0 $(($1-1))); do
  for k in 0 1 2; do
    e=${engines[$(( (t+k) % 3 ))]}
    rm -rf $W/w $W/s; cp -a --reflink=always $C $W/w; sync
    load=$(cut -d" " -f1 /proc/loadavg)
    r=$($B $e $W/w $W/s $2 $3 2>&1) || { echo "FAIL $e: $r" >&2; continue; }
    echo "$r" | python3 -c "import json,sys; d=json.loads(sys.stdin.read()); d['trial']=$t; d['load']=$load; print(json.dumps(d))" >> $OUT
    echo "trial $t $e done (load $load)"
  done
done
rm -rf $W/w $W/s
