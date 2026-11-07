#!/bin/zsh -f
# Store read/write measurements on the #415 code (line objects-bench).
set -u
L=/Users/igor/Projects/layr-store/projects/layr/lines/objects-bench
B=$L/target/release/examples/objects-bench; R=$L/target/release/examples/objects-read
O=$TMPDIR/ob; rm -rf $O; mkdir -p $O
now() { perl -MTime::HiRes=time -e 'printf "%.3f", time' }
echo "### History: last 60 commits of layr, load $(uptime | sed 's/.*averages: //')"
G=/Users/igor/Projects/layr-store/projects/layr/lines/main
H=$O/hstore; prev=""; : > $O/htrees; : > $O/htimes
for c in $(git -C $G log -60 --format=%H --reverse); do
  rm -rf $O/src; mkdir -p $O/src; git -C $G archive $c | tar -x -C $O/src
  out=$($B $H import $O/src cold ${prev:+--previous $prev})
  prev=$(echo $out | awk '{print $2}'); echo $prev >> $O/htrees; echo $out | awk '{print $4}' >> $O/htimes
done
echo "import done: $(tail -1 $O/htrees)"; $B $H stat | tail -1
s=$(now); $B $H repack | tail -1; e=$(now); echo "repack $(perl -e "printf '%.1f', $e-$s") s"; $B $H stat | tail -1
tot=0; i=0; worst=0
paste $O/htrees $O/htimes | while read t m; do i=$((i+1)); rm -rf $O/co; s=$(now); $B $H checkout $t $O/co --times $m >/dev/null; e=$(now); d=$(perl -e "print $e-$s"); tot=$(perl -e "print $tot+$d"); worst=$(perl -e "print $d>$worst?$d:$worst"); done
echo "checkout of 60 states from packs: total $(perl -e "printf '%.2f', $tot") s, worst $(perl -e "printf '%.3f', $worst") s"
c=$(git -C $G log -1 --format=%H); rm -rf $O/src; mkdir -p $O/src; git -C $G archive $c | tar -x -C $O/src
rm -rf $O/co; $B $H checkout $(tail -1 $O/htrees) $O/co --times $(tail -1 $O/htimes) >/dev/null; $B $H verify $O/src $O/co | tail -1
$R $H sample $O/htrees 300
$R $H decode $(tail -1 $O/htrees)
rm -rf $O/co $O/src
echo; echo "### Chromium src, load $(uptime | sed 's/.*averages: //')"
C=$TMPDIR/layr-chromium/src; S=$O/cstore
s=$(now); out=$($B $S import $C cold); e=$(now); echo "cold write: $(perl -e "printf '%.0f', $e-$s") s; $out" | cut -c1-200
ct=$(echo $out | awk '{print $2}'); cm=$(echo $out | awk '{print $4}')
s=$(now); $B $S repack | tail -1; e=$(now); echo "repack $(perl -e "printf '%.0f', $e-$s") s"; $B $S stat | tail -1
df -h ~ | tail -1
s=$(now); $B $S checkout $ct $O/cco --times $cm | tail -1; e=$(now); echo "checkout from packs: $(perl -e "printf '%.0f', $e-$s") s"
s=$(now); $B $S verify $C $O/cco --times | tail -1; e=$(now); echo "verify $(perl -e "printf '%.0f', $e-$s") s"
echo $ct > $O/ctrees; $R $S sample $O/ctrees 300; $R $S decode $ct
rm -rf $O/cco
# Save cost: hot changed write of 1 and 10 files (source untouched: the files are re-read as changed).
f1=$(cd $C && git ls-files | awk 'NR%50000==7' | head -1); f10=$(cd $C && git ls-files | awk 'NR%40000==3' | head -10 | tr '\n' ' ')
for n in 1 10; do ms=""; for r in 1 2 3 4 5; do files=$([ $n = 1 ] && echo $f1 || echo $f10); x=$(eval "$B $S changed $C $ct $cm $files" | grep -o 'in [0-9.]* ms' | awk '{print $2}'); ms="$ms $x"; done; echo "save of $n changed files: runs (ms):$ms"; done
echo "load $(uptime | sed 's/.*averages: //')"; du -sh $O | cut -f1
rm -rf $O
