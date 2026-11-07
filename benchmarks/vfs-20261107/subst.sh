#!/bin/zsh
# Tool substitution: native dir vs FSKit mount (openclose=off) vs daemon index over a socket.
set -u
export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
S=${0:A:h}; TD=$S/tooldaemon/target/release/tooldaemon
B=$TMPDIR/layrfs-subst; rm -rf $B; mkdir -p $B/src $B/mnt
cd /Users/igor/Projects/layr-studio; for f in $(ls -A | grep -v -x 'target'); do cp -cR "$f" $B/src/; done
echo "openclose=off" > ~/Library/Containers/dev.layr.fshost.fs/Data/layrfs.conf
mount -F -t layrfs $B/src $B/mnt || exit 1
SOCK=$B/td.sock; $TD serve $B/src $SOCK 2> $B/td.log & TDPID=$!; sleep 1; while [ ! -S $SOCK ]; do sleep 0.2; done; sleep 0.5; cat $B/td.log
now() { perl -MTime::HiRes=time -e 'printf "%.4f", time' }
typeset -A R
run() { local key=$1 dir=$2 cmd=$3 s e; cd $dir; s=$(now); eval "$cmd" < /dev/null > $B/out-$key 2>/dev/null; e=$(now); R[$key]="${R[$key]:-} $(perl -e "printf '%.3f', $e-$s")"; }
# Warm both sides once (FSKit cold walk, page cache).
for c in 'rg -c fn' 'rg --files' 'find . -type f' 'git status --porcelain'; do (cd $B/mnt && eval "$c" </dev/null >/dev/null 2>&1); (cd $B/src && eval "$c" </dev/null >/dev/null 2>&1); done
for i in 1 2 3 4 5; do
  run rg-native $B/src 'rg -c fn'
  run rg-fskit  $B/mnt 'rg -c fn'
  run rg-subst  $B/src "$TD client $SOCK rgc fn"
  run files-native $B/src 'rg --files'
  run files-fskit  $B/mnt 'rg --files'
  run files-subst  $B/src "$TD client $SOCK files"
  run find-native $B/src 'find . -type f'
  run find-fskit  $B/mnt 'find . -type f'
  run find-subst  $B/src "$TD client $SOCK find"
  run status-native $B/src 'git status --porcelain'
  run status-fskit  $B/mnt 'git status --porcelain'
  run status-subst  $B/src "$TD client $SOCK status"
done
med() { echo $* | tr ' ' '\n' | grep . | sort -n | sed -n 3p }
printf "%-8s %10s %10s %10s   (median of 5, seconds)\n" tool native fskit subst
for t in rg files find status; do printf "%-8s %10s %10s %10s\n" $t $(med ${R[$t-native]}) $(med ${R[$t-fskit]}) $(med ${R[$t-subst]}); done
echo "correctness (sorted output equal to native):"
for t in rg files find; do cmp -s <(sort $B/out-$t-native) <(sort $B/out-$t-subst) && echo "  $t subst: same ($(wc -l < $B/out-$t-native) lines)" || echo "  $t subst: DIFFERENT ($(wc -l < $B/out-$t-native) vs $(wc -l < $B/out-$t-subst))"; cmp -s <(sort $B/out-$t-native) <(sort $B/out-$t-fskit) && echo "  $t fskit: same" || echo "  $t fskit: DIFFERENT"; done
echo "load $(uptime | sed 's/.*averages: //')"
kill $TDPID; cd /; sleep 1; umount $B/mnt || diskutil unmount $B/mnt
