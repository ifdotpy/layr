#!/bin/zsh
# 1M-file tree: native APFS clone/delete/walks vs tool daemon vs FSKit (index=memory, openclose=off).
set -u
export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
S=${0:A:h}; TD=$S/tooldaemon/target/release/tooldaemon; G=$TMPDIR/layr-1m; SRC=$G/src
CONF=~/Library/Containers/dev.layr.fshost.fs/Data/layrfs.conf
now() { perl -MTime::HiRes=time -e 'printf "%.4f", time' }
t() { local s e; s=$(now); (cd $2 && eval "$3" < /dev/null > $G/out-$1 2>/dev/null); e=$(now); printf "%-34s %9.2f s  %s\n" "$1" $(perl -e "print $e-$s") "$(wc -l < $G/out-$1 | tr -d ' ') lines"; }
freek() { df -k $G | awk 'NR==2 {print $4}' }
echo "load $(uptime | sed 's/.*averages: //')"
echo "== native APFS"
t native-find-1     $SRC 'find . -type f'
t native-find-2     $SRC 'find . -type f'
t native-rgfiles    $SRC 'rg --files'
t native-rgc        $SRC 'rg -c fn'
f0=$(freek); s=$(now)
python3 -I -c "import ctypes,sys; l=ctypes.CDLL(None); r=l.clonefile(sys.argv[1].encode(),sys.argv[2].encode(),0); sys.exit(r)" $SRC $G/clone1
e=$(now); f1=$(freek); printf "%-34s %9.2f s  disk used %d MB\n" native-clonefile-dir $(perl -e "print $e-$s") $(( (f0-f1)/1024 ))
s=$(now); cp -cR $SRC $G/clone2; e=$(now); f2=$(freek); printf "%-34s %9.2f s  disk used %d MB\n" native-cp-c-R $(perl -e "print $e-$s") $(( (f1-f2)/1024 ))
s=$(now); rm -rf $G/clone1; e=$(now); printf "%-34s %9.2f s\n" native-rm-rf-clone $(perl -e "print $e-$s")
rm -rf $G/clone2
echo "== tool daemon (index in memory)"
SOCK=$G/td.sock; rm -f $SOCK; s=$(now); $TD serve $SRC $SOCK 2> $G/td.log & TDPID=$!
while [ ! -S $SOCK ]; do sleep 0.1; done; e=$(now); cat $G/td.log | head -1; printf "%-34s %9.2f s  rss %d MB\n" subst-index-build $(perl -e "print $e-$s") $(( $(ps -o rss= -p $TDPID) / 1024 ))
for i in 1 2; do t subst-find-$i $SRC "$TD client $SOCK find"; t subst-rgfiles-$i $SRC "$TD client $SOCK files"; t subst-rgc-$i $SRC "$TD client $SOCK rgc fn"; done
cmp -s <(sort $G/out-native-rgc) <(sort $G/out-subst-rgc-2) && echo "subst rg -c output: same as native" || echo "subst rg -c output: DIFFERENT"
kill $TDPID
fskit() { # fskit <label> <conf>
  echo "== FSKit $1 ($2)"; echo "$2" | tr ' ' '\n' > $CONF
  mkdir -p $G/mnt; s=$(now); mount -F -t layrfs $SRC $G/mnt || { echo "mount failed"; return; }; e=$(now)
  local P=$(pgrep -nx LayrFS); printf "%-34s %9.2f s  module rss %d MB\n" fskit-$1-mount $(perl -e "print $e-$s") $(( $(ps -o rss= -p $P) / 1024 ))
  t fskit-$1-find-cold $G/mnt 'find . -type f'
  t fskit-$1-find-warm $G/mnt 'find . -type f'
  if [[ $1 != null ]]; then
    t fskit-$1-rgfiles $G/mnt 'rg --files'
    t fskit-$1-rgc     $G/mnt 'rg -c fn'
    t fskit-$1-rgc-2   $G/mnt 'rg -c fn'
  fi
  printf "%-34s module rss %d MB\n" fskit-$1-after $(( $(ps -o rss= -p $P) / 1024 ))
  kill -USR1 $P; sleep 1; cp ~/Library/Containers/dev.layr.fshost.fs/Data/tmp/layrfs-stats-latest.json $G/stats-$1.json 2>/dev/null
  cd /; sleep 2; umount $G/mnt 2>/dev/null || diskutil unmount $G/mnt >/dev/null
}
fskit disk  "openclose=off index=disk"
fskit mem   "openclose=off index=memory"
fskit null  "openclose=off index=memory null_enumerate_probe=1"
rm -f $CONF
echo "load $(uptime | sed 's/.*averages: //')"
