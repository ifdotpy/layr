#!/bin/bash
# E3: a saved state after every agent step, and a jump back (LayrFS) against Btrfs.
. ~/lfx/lab/lab.sh
S=${S:-50} K=${K:-20} T=lab$$
files() { sed -n "$((20000 + $1 * K + 1)),$((20000 + $1 * K + K))p" $LIST; }
vnew ck linux-base
P=$(M ck)
cat $P/Makefile >/dev/null
sv=(); ed=()
for i in $(seq $S); do
  t0=$(now); files $i | while read -r f; do printf '// step %d\n' $i >> "$P/$f"; done; t1=$(now); ed+=($(ms $t0 $t1))
  t0=$(now); vctl ck save $T-ck-$i >/dev/null; t1=$(now); sv+=($(ms $t0 $t1))
done
rec e3 "fuse: edit $K files (median)" "$(printf '%s\n' "${ed[@]}" | median)"
rec e3 "fuse: save after each step (median of $S)" "$(printf '%s\n' "${sv[@]}" | median)" "\"all\":\"${sv[*]}\""
t0=$(now); out=$(vctl ck goto $T-ck-10); t1=$(now)
rec e3 "fuse: goto step 10 in place" $(ms $t0 $t1) "\"out\":\"$(echo $out | tr '"' "'")\""
f10=$(files 10 | head -1); f11=$(files 11 | head -1)
tail -1 "$P/$f10" | grep -q 'step 10' && ! grep -q 'step 11' "$P/$f11" && echo "e3 check: content of step 10 OK" || { echo "e3 check FAILED"; exit 1; }
t0=$(now); vctl ck goto $T-ck-$S >/dev/null; t1=$(now); rec e3 "fuse: goto step $S in place" $(ms $t0 $t1)
grep -q "step $S" "$P/$(files $S | head -1)" && echo "e3 check: content of step $S OK"
vdel ck
# Btrfs: the save of bench.sh (read-only snapshot, send --no-data, write the changed files as a state).
L2=$B/btrfs/lines/e3; sudo btrfs subvolume snapshot $B/btrfs/linux-base $L2 >/dev/null
bsave() { # bsave <base state> <new state>
  local base=$1 new=$2 p=$B/btrfs/states/$2
  sudo btrfs subvolume snapshot -r $L2 $p >/dev/null
  sudo btrfs send -q --no-data -p $B/btrfs/states/$base $p | btrfs receive --dump > $B/tmp/$new.dump
  awk '{print $2; for (i = 3; i <= NF; i++) if ($i ~ /^dest=/) print substr($i, 6)}' $B/tmp/$new.dump |
    sed 's|^\./||; s|^[^/]*/||' | grep -v '^o[0-9]*-[0-9]*-0$' | LC_ALL=C sort -u |
    while read -r f; do [ -d "$p/$f" ] || echo "$f"; done > $B/tmp/$new.paths
  $LF write-changed $B/btrfs/objects $base $p $B/tmp/$new.paths $new >/dev/null
}
sv=(); sn=(); prev=linux-base
for i in $(seq $S); do
  files $i | while read -r f; do printf '// step %d\n' $i >> "$L2/$f"; done
  t0=$(now); bsave $prev $T-b-$i; t1=$(now); sv+=($(ms $t0 $t1)); prev=$T-b-$i
done
rec e3 "btrfs: save as a layr state after each step (median of $S)" "$(printf '%s\n' "${sv[@]}" | median)" "\"all\":\"${sv[*]}\""
for i in $(seq 5); do t0=$(now); sudo btrfs subvolume snapshot -r $L2 $B/btrfs/states/$T-snap-$i >/dev/null; t1=$(now); sn+=($(ms $t0 $t1)); done
rec e3 "btrfs: read-only snapshot only, no state (median of 5)" "$(printf '%s\n' "${sn[@]}" | median)"
t0=$(now); sudo btrfs subvolume delete $L2 >/dev/null; sudo btrfs subvolume snapshot $B/btrfs/states/$T-b-10 $L2 >/dev/null; t1=$(now)
rec e3 "btrfs: back to step 10 (delete + snapshot)" $(ms $t0 $t1)
grep -q 'step 10' "$L2/$f10" && ! grep -q 'step 11' "$L2/$f11" && echo "e3 check btrfs OK"
sudo btrfs subvolume delete $L2 >/dev/null
for p in $B/btrfs/states/$T-*; do sudo btrfs subvolume delete $p >/dev/null; done
