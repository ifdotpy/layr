#!/bin/bash
# Times of the built overlays on a tree of 10,000 files: commits with and without an overlay,
# status, save and a new line. Run as root on Linux with btrfs and loop devices:
#   cargo build --release && LAYR=$PWD/target/release/layr docs/overlays-bench/commit.sh
. "$(dirname "$0")/../../tests/lib.sh"
mkdir -p $T/big
for d in $(seq 1 100); do mkdir -p $T/big/d$d; for f in $(seq 1 100); do echo "$d $f" > $T/big/d$d/f$f.txt; done; done
l init big --from $T/big -q >/dev/null
P=$LAYR_ROOT/projects/big/lines
for n in 10 1000; do
  l --project big branch -q plain$n main
  l --project big branch -q stacked$n main
  ( cd $P/stacked$n || exit 1
    l overlay create -q priv$n
    if [ $n = 10 ]; then l overlay move -q priv$n d1/f1.txt d1/f2.txt d1/f3.txt d1/f4.txt d1/f5.txt d1/f6.txt d1/f7.txt d1/f8.txt d1/f9.txt d1/f10.txt
    else l overlay move -q priv$n d1 d2 d3 d4 d5 d6 d7 d8 d9 d10; fi
    s=$(date +%s%N); l commit -qm priv; e=$(date +%s%N); echo "first commit into an overlay of $n files: $(( (e-s)/1000000 )) ms" )
done
ms() { local a b; a=$(date +%s%N); "$@" >/dev/null; b=$(date +%s%N); echo $(( (b - a) / 1000000 )); }
for n in 10 1000; do
  for r in 1 2 3 4 5; do
    for line in plain$n stacked$n; do
      ( cd $P/$line || exit 1; for i in 1 2 3 4 5; do echo "$r" >> d1/f$i.txt; echo "$r" >> d50/f$i.txt; done )
    done
    btrfs filesystem sync $LAYR_ROOT >/dev/null
    tp=$(cd $P/plain$n && ms l commit -qam "r$r")
    btrfs filesystem sync $LAYR_ROOT >/dev/null
    ts=$(cd $P/stacked$n && ms l commit -qam "r$r")
    echo "overlay of $n files, run $r: commit of 10 changed files (5 shared, 5 in the overlay): plain ${tp} ms, with the overlay ${ts} ms"
  done
done
cd $P/stacked1000 && for i in 1 2 3 4 5; do echo t >> d1/f$i.txt; echo t >> d50/f$i.txt; done && LAYR_TRACE=1 l commit -qam traced 2>&1 | sort -k2 -n -r | head -12
for line in plain1000 stacked1000; do cd $P/$line; echo "status of $line: $(ms l status --porcelain) ms, $(ms l status --porcelain) ms"; done
cd $P/stacked1000 && echo "save of stacked1000: $(ms l save --force) ms; plain: $(cd $P/plain1000 && ms l save --force) ms"
cd /tmp; s=$(date +%s%N); l --project big branch -q newline stacked1000; e=$(date +%s%N); echo "branch from the stacked line (assembly of 1,000 overlay files): $(( (e-s)/1000000 )) ms"
s=$(date +%s%N); l --project big branch -q newplain plain1000; e=$(date +%s%N); echo "branch from the plain line: $(( (e-s)/1000000 )) ms"
echo "fsck: $(l --project big fsck | tail -1)"
