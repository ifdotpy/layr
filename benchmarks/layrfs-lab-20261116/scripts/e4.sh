#!/bin/bash
# E4: rebase a line onto a new upstream state (LayrFS, three-way merge of tree objects) against Btrfs.
. ~/lfx/lab/lab.sh
T=lab$$ A=${A:-200}
up() { # upstream changes in folder $1: 3000 edits, 200 new files, 200 removals
  sed -n '30001,33000p' $LIST | while read -r f; do printf '// upstream\n' >> "$1/$f"; done
  mkdir -p "$1/up-new"; for j in $(seq 200); do echo "new $j" > "$1/up-new/f$j"; done
  sed -n '33001,33200p' $LIST | while read -r f; do rm "$1/$f"; done
}
agent() { sed -n "40001,$((40000 + A))p" $LIST | while read -r f; do printf '// agent\n' >> "$1/$f"; done; }
if [ -z "${SKIP_FUSE:-}" ]; then
vnew up linux-base; up $(M up); vctl up save $T-B >/dev/null; vdel up
vnew ag linux-base; agent $(M ag)
t0=$(now); out=$(vctl ag rebase $T-B $T-R); t1=$(now)
rec e4 "fuse: rebase $A agent changes onto upstream" $(ms $t0 $t1) "\"out\":\"$(echo $out | tr '"' "'")\""
P=$(M ag)
grep -q upstream "$P/$(sed -n 30001p $LIST)" && grep -q agent "$P/$(sed -n 40001p $LIST)" && [ -e "$P/up-new/f1" ] && [ ! -e "$P/$(sed -n 33001p $LIST)" ] &&
  echo "e4 check: content OK" || { echo "e4 check FAILED"; exit 1; }
rtree=$(echo "$out" | grep -o '"tree":"[0-9a-f]*"')
vdel ag
vnew ch $T-B; agent $(M ch); ctree=$(vctl ch save $T-R2 | grep -o '"tree":"[0-9a-f]*"'); vdel ch
[ "$rtree" = "$ctree" ] && echo "e4 check: tree equal to the edits made on upstream ($rtree)" || { echo "e4 tree differs: $rtree $ctree"; exit 1; }
vnew cf linux-base; f=$(sed -n 30001p $LIST); printf '// agent\n' >> "$(M cf)/$f"
t0=$(now); out=$(vctl cf rebase $T-B $T-RC 2>&1 || true); t1=$(now)
rec e4 "fuse: rebase with 1 conflict" $(ms $t0 $t1) "\"out\":\"$(echo $out | tr '"' "'" | head -c 300)\""
vdel cf
fi
# Btrfs: upstream as a subvolume; rebase = read-only snapshot, send --no-data for the changed paths,
# conflict check with cmp against base and upstream, snapshot of upstream, copy of the changed files.
U=$B/btrfs/lines/e4-up; A2=$B/btrfs/lines/e4-ag; N2=$B/btrfs/lines/e4-new
sudo btrfs subvolume snapshot $B/btrfs/linux-base $U >/dev/null; up $U
sudo btrfs subvolume snapshot $B/btrfs/linux-base $A2 >/dev/null; agent $A2
t0=$(now)
sudo btrfs subvolume snapshot -r $A2 $B/btrfs/states/$T-ag >/dev/null
sudo btrfs send -q --no-data -p $B/btrfs/states/linux-base $B/btrfs/states/$T-ag | btrfs receive --dump > $B/tmp/$T.dump
awk '{print $2}' $B/tmp/$T.dump | sed 's|^\./||; s|^[^/]*/||' | LC_ALL=C sort -u | while read -r f; do if [ -f "$A2/$f" ]; then echo "$f"; fi; done > $B/tmp/$T.paths
c=0; while read -r f; do cmp -s "$B/btrfs/linux-base/$f" "$U/$f" || c=$((c+1)); done < $B/tmp/$T.paths
sudo btrfs subvolume snapshot $U $N2 >/dev/null
sudo chown $me $N2
while read -r f; do cp --reflink=always "$A2/$f" "$N2/$f"; done < $B/tmp/$T.paths
t1=$(now)
rec e4 "btrfs: rebase $A agent changes (no state saved)" $(ms $t0 $t1) "\"paths\":$(wc -l < $B/tmp/$T.paths),\"conflicts\":$c"
for p in $U $A2 $N2 $B/btrfs/states/$T-ag; do sudo btrfs subvolume delete $p >/dev/null; done
