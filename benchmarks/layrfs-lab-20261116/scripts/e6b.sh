#!/bin/bash
# E6b: why a delete of a Linux kernel line takes 200 ms: the walk of the index leaves 97,000 inodes in the kernel.
. ~/lfx/lab/lab.sh
a=(); b=()
for i in 1 2 3 4 5; do
  vnew d$i linux-base >/dev/null
  t0=$(now); vctl d$i stop remove >/dev/null; t1=$(now); a+=($(ms $t0 $t1)); rm -rf $B/caches/d$i
  vnew d$i linux-base >/dev/null
  until vctl d$i stats | grep -q '"index_entries":[0-9]'; do sleep 0.02; done
  t0=$(now); vctl d$i stop remove >/dev/null; t1=$(now); b+=($(ms $t0 $t1)); rm -rf $B/caches/d$i
done
rec e6 "linux: delete before the index walk ends (median of 5)" "$(printf '%s\n' "${a[@]}" | median)" "\"all\":\"${a[*]}\""
rec e6 "linux: delete after the index walk (median of 5)" "$(printf '%s\n' "${b[@]}" | median)" "\"all\":\"${b[*]}\""
