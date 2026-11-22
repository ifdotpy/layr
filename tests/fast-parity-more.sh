#!/bin/bash
# Feature parity of fast lines with classic lines, second part (tests/fast-parity.sh is the first):
# store remotes, cache overlays (the caches of a fast line live in lines/.caches/<id>, a link shows
# each), checks, the queue and retest. The same commands on a line of each kind must give the same
# exit codes and output. The retest steps need cargo and cargo-nextest; the same environment as the
# first part.
set -u
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
[ "$(uname -s)" = Darwin ] || : "${LAYR_SOCKET:?}"
export LAYR_AUTHOR_NAME=Parity LAYR_AUTHOR_EMAIL=parity@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-parity2.XXXXXX")" && pwd -P)
pp=fastparity2-$(date +%s)-$$
src=$work/source; mkdir -p $src/src
printf 'target regenerable\n' > $src/.layrattributes
printf 'target/\n.layr-retest/\nCargo.lock\n' > $src/.gitignore
printf '[package]\nname = "parity"\nversion = "0.1.0"\nedition = "2021"\n' > $src/Cargo.toml
printf 'pub fn one() -> u8 { 1 }\n#[test]\nfn one_is_one() { assert_eq!(one(), 1) }\n' > $src/src/lib.rs
# A project for each kind: remotes, the queue and pushes are of the project.
for kind in classic fast; do
  p=$pp-$kind
  $L init $p --from $src -q >/dev/null
  cd $LAYR_ROOT/projects/$p/lines/main
  $L protect main --remove >/dev/null
  $L migrate --delete-old >/dev/null
  base=$($L rev-parse head)
  n=$kind; out=$work/$kind.txt; : > $out
  $L line new $n $base --$kind -q >/dev/null || { echo "line new --$kind failed" >&2; exit 1; }
  cd $LAYR_ROOT/projects/$p/lines/$n
  t() {
    local name=$1; shift; local o rc
    o=$("$@" 2>&1); rc=$?
    printf '== %s rc=%s\n%s\n' "$name" $rc "$o" | R="$LAYR_ROOT" W="$work" P="$p" N="$n" perl -pe 's/\Q$ENV{R}\E/ROOT/g; s/\Q$ENV{W}\E/WORK/g; s/\Q$ENV{P}\E/PROJECT/g; s/\b[0-9a-f]{7,64}(-[0-9a-f]{4}){3}-[0-9a-f]{12}\b/ID/g; s/\b[0-9a-f]{7,64}\b/ID/g; s/\b[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-ID\b/ID/g; s/\b\Q$ENV{N}\E\b/LINE/g; s/\d{4}-\d\d-\d\d [\d:]{8} [+-]\d{4}/DATE/g; s/^Date: .*/Date: DATE/; s/\b\d+(\.\d+)?m?s\b/TIME/g; s/\b\d+ bytes\b/N bytes/g' >> $out
  }
  # Store remote.
  printf 'remote\n' > r1; $L add r1 >/dev/null; $L save -qm remote
  t "remote add" $L remote add arch file://$work/arch-$n --paper-key-out $work/paper-$n
  t "push" $L push arch
  t "fsck --remote" $L fsck --remote arch --files
  t "log --remote" $L log --remote arch -n 2
  printf 'second\n' >> r1; $L save -aqm second
  t "push again" $L push arch
  # The count of states that a fetch restores is the states with no local tree (`states/<id>`); a fast line's
  # snapshots leave such trees, so it restores fewer. The content is the same.
  t "fetch" sh -c "$L fetch arch | sed 's/[0-9]* states/N states/'"
  t "pull" $L pull arch
  t "sync" $L sync arch
  # A gc between two reads of the line: the objects of the last read can go (Linux shadow).
  printf 'gc1\n' >> r1; $L status >/dev/null 2>&1; $L gc >/dev/null 2>&1; echo "== gc rc=$?" >> $out
  printf 'gc2\n' >> r1
  t "status after gc" $L status --porcelain
  t "diff after gc" $L diff r1
  $L save -aqm gc
  # Cache overlays.
  mkdir -p target/debug; echo bin1 > target/debug/x
  t "save --overlays" $L save --overlays
  t "overlay list" sh -c "$L overlay list | cut -f1,2,6,7"
  t "status with a cache" $L status --porcelain
  t "cache content" cat target/debug/x
  t "line new from it" $L line new c-$n -q
  t "child c has the cache" cat ../c-$n/target/debug/x
  t "line new f from it" $L line new f-$n --fast -q
  t "child f has the cache" cat ../f-$n/target/debug/x
  # A removed cache folder: layr makes it again (cold) before a tool writes in it (a plain folder
  # that a tool makes at a cache path is refused while a build may run in it).
  rm -rf target; $L status >/dev/null 2>&1; $L save --overlays -q >/dev/null 2>&1
  mkdir -p target/debug; echo bin2 > target/debug/y
  t "save --overlays after rm" $L save --overlays
  t "cache after rm" ls target/debug
  t "status after rm" $L status --porcelain
  (cd ../c-$n && echo bin3 > target/debug/z && $L save --overlays -q)
  # A moved cache folder keeps its files; a new one at the old place starts empty.
  mv target target-old; $L save --overlays -q >/dev/null 2>&1; mkdir -p target/debug; echo bin4 > target/debug/w
  t "save --overlays after mv" $L save --overlays
  t "moved cache keeps its files" ls target-old/debug
  t "new cache at the old place" ls target/debug
  rm -rf target-old
  t "merge brings caches" $L merge c-$n -m caches
  t "moved caches left" sh -c "ls -A $LAYR_ROOT/projects/$p/lines/.caches/*/.moved 2>/dev/null | wc -l | tr -d ' '"
  t "cache after merge" ls target/debug
  t "line delete children" $L line delete -D c-$n f-$n
  # Checks.
  $L config --local checks.runner true >/dev/null
  t "check config" $L config check.pass 'test -f Cargo.toml && cat src/lib.rs >/dev/null'
  t "checks.changes" $L config checks.changes '["pass"]'
  t "checks run" sh -c "$L checks run --state $n --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"count\"], d[\"pending\"], [c.get(\"status\") or c.get(\"result\") or c.get(\"exit_code\") for c in d[\"checks\"]])'"
  t "try --check" $L try head --check pass
  t "checks pending" sh -c "$L checks pending --state $n --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)[\"pending\"]))'"
  $L config checks.changes '[]' >/dev/null
  # Queue: from this line into another, and from another line into this one.
  $L line new q-$n $base -q
  $L config line.$n.upstream q-$n
  printf 'queued\n' > q1; $L add q1 >/dev/null; $L save -qm queued
  t "queue add" sh -c "$L queue add head --json >/dev/null && $L queue list --json | python3 -c 'import json,sys; print([e[\"status\"] for e in json.load(sys.stdin)])'"
  (cd ../q-$n && $L queue run --into q-$n >/dev/null 2>&1; echo "into other rc=$?"; cat q1) > $work/q-$n.txt 2>&1
  t "queue into other" cat $work/q-$n.txt
  $L line new s-$n $base -q
  (cd ../s-$n && printf 'from s\n' > s1 && $L add s1 >/dev/null && $L save -qm s && $L config line.s-$n.upstream $n && $L queue add head >/dev/null)
  t "queue run into this" $L queue run --into $n
  t "queued file here" cat s1
  t "status after queue" $L status --porcelain
  t "log after queue" $L log -n 2
  t "line delete queue lines" $L line delete -D q-$n s-$n
  # Retest (needs cargo-nextest; without it both kinds skip the same steps).
  if command -v cargo-nextest >/dev/null 2>&1; then
    t "retest run" sh -c "$L retest run 2>&1 | grep -o 'ran [0-9]*, passed [0-9]*, failed [0-9]*'"
    t "status after retest" $L status --porcelain
    t "save after retest" $L save -a -m retest
  fi
  # A user attribute comes back with the file (`restore` sets it).
  if [ "$(uname -s)" = Darwin ]; then
    xset() { xattr -w "$2" "$3" "$1"; }; xdel() { xattr -d "$2" "$1"; }; xlist() { xattr "$1" | grep '^user\.' | sort; }
  else
    xset() { python3 -c "import os,sys; os.setxattr(sys.argv[1], sys.argv[2], sys.argv[3].encode())" "$@"; }
    xdel() { python3 -c "import os,sys; os.removexattr(sys.argv[1], sys.argv[2])" "$@"; }
    xlist() { python3 -c "import os,sys; print('\n'.join(sorted(a for a in os.listxattr(sys.argv[1]) if a.startswith('user.'))))" "$1"; }
  fi
  printf 'x\n' > xa; xset xa user.test v1
  $L add xa >/dev/null; $L save -qm xattr
  printf 'y\n' > xa; xdel xa user.test
  t "restore with a user attribute" $L restore xa
  t "attribute after restore" sh -c "cat xa"
  t "attribute list after restore" xlist xa
  # A folder of the start state removed and made again: its old files must not come back.
  rm -rf src; mkdir src; printf 'other\n' > src/other.rs
  t "add -A after a folder is made again" $L add -A
  t "status after a folder is made again" $L status --porcelain
  t "save after a folder is made again" $L save -m again
  t "files of the folder made again" $L ls-files src
  printf 'unsaved\n' > unsaved1
  # A staged version and a newer one: the delete keeps both (its final states), undo brings them back.
  printf 'A\n' > staged1; $L add staged1 >/dev/null; printf 'B\n' > staged1
  cd $LAYR_ROOT/projects/$p/lines/main
  t "line delete with unsaved" $L line delete -D $n
  t "line delete --discard" $L line delete -D --discard $n
  t "undo of the delete" sh -c "$L undo >/dev/null 2>&1; echo \$?"
  t "unsaved file back" cat $LAYR_ROOT/projects/$p/lines/$n/unsaved1
  t "staged version back" sh -c "cd $LAYR_ROOT/projects/$p/lines/$n && $L show :staged1"
  t "newer version back" cat $LAYR_ROOT/projects/$p/lines/$n/staged1
  t "line delete again" $L line delete -D --discard $n
  t "no cache root left" sh -c "ls -A $LAYR_ROOT/projects/$p/lines/.caches 2>/dev/null | wc -l | tr -d ' '"
done
if diff -u $work/classic.txt $work/fast.txt; then
  echo "PASS: fast lines match classic lines ($(grep -c '^==' $work/fast.txt) commands)"
else
  echo "FAIL: fast lines differ from classic lines (above); evidence in $work" >&2; exit 1
fi
