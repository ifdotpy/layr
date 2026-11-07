#!/bin/zsh
# Real Chromium src: native APFS folder vs tool daemon (index in memory).
set -u
export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
S=${0:A:h}; TD=$S/tooldaemon/target/release/tooldaemon; C=$TMPDIR/layr-chromium; SRC=$C/src
now() { perl -MTime::HiRes=time -e 'printf "%.4f", time' }
typeset -A R
run() { local s e; s=$(now); (cd $SRC && eval "$2" < /dev/null > $C/out-$1 2>/dev/null); e=$(now); R[$1]="${R[$1]:-} $(perl -e "printf '%.3f', $e-$s")"; }
med() { echo $* | tr ' ' '\n' | grep . | sort -n | sed -n 2p }
freek() { df -k $C | awk 'NR==2 {print $4}' }
echo "load $(uptime | sed 's/.*averages: //')"
SOCK=$C/td.sock; rm -f $SOCK; s=$(now); $TD serve $SRC $SOCK 2> $C/td.log & TDPID=$!
while [ ! -S $SOCK ]; do sleep 0.1; done; e=$(now)
printf "daemon index build: %.1f s, rss %d MB  (%s)\n" $(perl -e "print $e-$s") $(( $(ps -o rss= -p $TDPID) / 1024 )) "$(head -1 $C/td.log)"
# warm page cache and git index once
(cd $SRC && git status --porcelain >/dev/null 2>&1; rg -c TODO >/dev/null 2>&1 </dev/null)
for i in 1 2 3; do
  run find-native   'find . -type f -not -path "./.git/*"'
  run find-subst    "$TD client $SOCK find"
  run files-native  'rg --files'
  run files-subst   "$TD client $SOCK files"
  run rgc-native    'rg -c TODO'
  run rgc-subst     "$TD client $SOCK rgc TODO"
  run status-native 'git status --porcelain'
  run status-subst  "$TD client $SOCK status"
done
printf "%-8s %10s %10s %8s   (median of 3, s)\n" tool native subst ratio
for t in find files rgc status; do n=$(med ${R[$t-native]}); u=$(med ${R[$t-subst]}); printf "%-8s %10s %10s %7.1fx\n" $t $n $u $(perl -e "print $n/$u"); done
echo "correctness:"
for t in files rgc; do cmp -s <(sort $C/out-$t-native) <(sort $C/out-$t-subst) && echo "  $t: same ($(wc -l < $C/out-$t-native | tr -d ' ') lines)" || echo "  $t: DIFFERENT ($(wc -l < $C/out-$t-native | tr -d ' ') vs $(wc -l < $C/out-$t-subst | tr -d ' '))"; done
echo "  find: native $(wc -l < $C/out-find-native | tr -d ' ') vs subst $(grep -v '^\./\.git/' $C/out-find-subst | wc -l | tr -d ' ') (subst without .git)"
kill $TDPID
echo "native branch cost:"
f0=$(freek); s=$(now); python3 -I -c "import ctypes,sys; sys.exit(ctypes.CDLL(None).clonefile(sys.argv[1].encode(),sys.argv[2].encode(),0))" $SRC $C/clone1; e=$(now); f1=$(freek)
printf "  clonefile whole tree: %.1f s, disk %d MB\n" $(perl -e "print $e-$s") $(( (f0-f1)/1024 ))
s=$(now); rm -rf $C/clone1; e=$(now); printf "  rm -rf clone: %.1f s\n" $(perl -e "print $e-$s")
echo "load $(uptime | sed 's/.*averages: //')"
