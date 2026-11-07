#!/bin/zsh
# M1: interleaved cargo build native vs FSKit (openclose=off), 3 rounds.
# M2: git status, edit loop, rename through FSKit vs native.
set -u
export PATH="$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
B=$TMPDIR/layrfs-m12; rm -rf $B; mkdir -p $B/mnt $B/t
mkdir -p $B/src; cd /Users/igor/Projects/layr; for f in $(ls -A | grep -v -x -E 'target|\.layr-retest'); do cp -cR "$f" $B/src/; done
CONF=~/Library/Containers/dev.layr.fshost.fs/Data/layrfs.conf; echo "openclose=off" > $CONF
(cd $B/src && cargo fetch -q)
mount -F -t layrfs $B/src $B/mnt || exit 1
t() { local s=$(perl -MTime::HiRes=time -e 'print time'); eval "$2" >/dev/null 2>&1; local e=$(perl -MTime::HiRes=time -e 'print time'); printf "%-28s %8.2f\n" "$1" $(( e - s )); }
echo "load $(uptime | sed 's/.*averages: //')"
for r in 1 2 3; do
  rm -rf $B/t/n $B/t/m
  t "cargo-native-$r" "cd $B/src && CARGO_TARGET_DIR=$B/t/n cargo build --offline -q || echo FAIL >&2"
  t "cargo-fskit-$r"  "cd $B/mnt && CARGO_TARGET_DIR=$B/t/m cargo build --offline -q"
done
t "cargo-noop-native" "cd $B/src && CARGO_TARGET_DIR=$B/t/n cargo build --offline -q"
t "cargo-noop-fskit"  "cd $B/mnt && CARGO_TARGET_DIR=$B/t/m cargo build --offline -q"
for side in src mnt; do
  D=$B/$side
  t "git-status-1-$side" "cd $D && git status --porcelain"
  t "git-status-2-$side" "cd $D && git status --porcelain"
  t "edit200-$side" "cd $D && for i in {1..200}; do echo \$i >> src/main.rs; done; git checkout -q -- src/main.rs"
  t "create-rename-rm-500-$side" "cd $D && mkdir -p zz && for i in {1..500}; do echo x > zz/f\$i.tmp && mv zz/f\$i.tmp zz/f\$i; done && rm -rf zz"
  t "git-add-commit-$side" "cd $D && echo // >> src/lib.rs && git -c user.name=b -c user.email=b@x add src/lib.rs && git -c user.name=b -c user.email=b@x commit -qm t && git reset -q --hard HEAD~1"
done
t "cargo-touch1-native" "cd $B/src && touch src/lib.rs && CARGO_TARGET_DIR=$B/t/n cargo build --offline -q"
t "cargo-touch1-fskit"  "cd $B/mnt && touch src/lib.rs && CARGO_TARGET_DIR=$B/t/m cargo build --offline -q"
echo "load $(uptime | sed 's/.*averages: //')"
sleep 2; umount $B/mnt || diskutil unmount $B/mnt; rm -f $CONF
