#!/bin/bash
# Feature parity of fast lines (LayrFS) with classic lines on Linux: the same commands on a line of
# each kind, from the same state, must give the same exit codes and the same output (state ids and
# line names aside). Provide LAYR (on Linux built with layrfs-fuse), an isolated LAYR_ROOT, and on
# Linux LAYR_SOCKET of its root daemon. Run as a non-root user. macOS needs the LayrFS extension.
set -u
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
# Linux: LAYR_SOCKET of the store's root daemon. macOS: commands run in the caller's process.
[ "$(uname -s)" = Darwin ] || : "${LAYR_SOCKET:?}"
export LAYR_AUTHOR_NAME=Parity LAYR_AUTHOR_EMAIL=parity@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-parity.XXXXXX")" && pwd -P)
p=fastparity-$(date +%s)-$$
src=$work/source; mkdir -p $src/d
for i in 1 2 3 4 5; do printf 'line %s\n' $i > $src/f$i; done
printf 'x\n' > $src/d/g; printf 'build/\n' > $src/.gitignore
$L init $p --from $src -q >/dev/null
cd $LAYR_ROOT/projects/$p/lines/main
$L protect main --remove >/dev/null
$L migrate --delete-old >/dev/null
printf 'main change\n' >> f5; $L save -a -m "main change" -q >/dev/null
base=$($L rev-parse head~1)
# The project gets its git store before either line starts: a line made after it has a `.git`
# folder, of either kind, so both lines run the same commands with one.
$L git sync -q >/dev/null 2>&1
for kind in classic fast; do
  n=$kind; out=$work/$kind.txt; : > $out
  $L line new $n $base --$kind -q >/dev/null || { echo "line new --$kind failed" >&2; exit 1; }
  cd $LAYR_ROOT/projects/$p/lines/$n
  t() {
    local name=$1; shift; local o rc
    o=$("$@" 2>&1); rc=$?
    printf '== %s rc=%s\n%s\n' "$name" $rc "$o" | R="$LAYR_ROOT" W="$work" P="$p" N="$n" perl -pe 's/\Q$ENV{R}\E/ROOT/g; s/\Q$ENV{W}\E/WORK/g; s/\Q$ENV{P}\E/PROJECT/g; s/\b[0-9a-f]{7,64}\b/ID/g; s/\b\Q$ENV{N}\E2\b/LINE2/g; s/\b\Q$ENV{N}\E\b/LINE/g; s/\d{4}-\d\d-\d\d [\d:]{8} [+-]\d{4}/DATE/g' >> $out
  }
  printf 'edit\n' >> f1; printf 'new\n' > n1
  t status $L status --porcelain
  t add $L add f1
  t "status after add" $L status --porcelain
  t "diff --cached" $L diff --cached
  t "save staged" $L save -m staged
  t rm $L rm f2
  t mv $L mv f3 f3b
  t "save rm mv" $L save -m rmmv
  printf 'x\n' >> f4; t restore $L restore f4
  t grep $L grep line
  t ls-files $L ls-files
  t blame $L blame f1
  printf 'park me\n' >> f4; t park $L park
  t "park list" $L park list
  t "park pop" $L park pop
  t "save -a" $L save -a -m all
  t "merge main" $L merge main -m merged
  t "status in conflict" $L status --porcelain
  t "reset --hard" $L reset --hard
  t "log" $L log --oneline -n 3
  t invert $L invert head
  t pick $L pick main
  t undo $L undo
  printf 'junk\n' >> f1; t "reset --hard 2" $L reset --hard
  mkdir -p build; echo o > build/out; printf 'u\n' > untracked1
  t "clean -n" $L clean -n
  t "clean -f" $L clean -f
  t "status final" $L status --porcelain
  t "show head:f1" $L show head:f1
  # Second batch: patches, history commands, export, git, tools, moves.
  printf 'patched\n' >> f4; $L diff > $work/$n.patch; $L reset --hard -q >/dev/null 2>&1
  t "apply" $L apply $work/$n.patch
  t "status after apply" $L status --porcelain
  t "save after apply" $L save -a -m patched
  t "tag" $L tag t-$n
  t "show tag" $L show t-$n:f4
  t "replay main" $L replay main
  t "try" $L try head -- cat f4
  # The exit code only: the second run reuses the check results of the first.
  $L bisect run --good $base --bad head -- sh -c 'grep -q patched f4 && exit 1 || exit 0' >/dev/null 2>&1; echo "== bisect rc=$?" >> $out
  t "export" $L export head $work/exp-$n
  t "export content" cat $work/exp-$n/f4
  t "tool rg" $L tool rg -l patched .
  t "tool find" $L tool find . -name f4
  t "git sync" $L git sync -q
  git init -q --bare $work/$n.git
  t "git remote add" $L git remote add o-$n $work/$n.git
  t "git push" $L git push o-$n
  t "git log of push" git --git-dir=$work/$n.git log --all -n 1 --format=%s
  t "keep list" $L keep list
  cd $LAYR_ROOT/projects/$p/lines/main
  t "line move" $L line move $n $work/moved-$n
  cd $work/moved-$n 2>/dev/null && { printf 'moved\n' >> f5; t "status after move" $L status --porcelain; t "save after move" $L save -a -m moved; }
  cd $LAYR_ROOT/projects/$p/lines/main
  t "line move back" $L line move $n
  t "line rename" $L line rename $n ${n}2
  cd $LAYR_ROOT/projects/$p/lines/${n}2 2>/dev/null || cd $LAYR_ROOT/projects/$p/lines/$n
  printf 'renamed\n' >> f5
  t "status after rename" $L status --porcelain
  t "save after rename" $L save -a -m renamed
  cd $LAYR_ROOT/projects/$p/lines/main
  t "line delete" $L line delete -D ${n}2
done
if diff -u $work/classic.txt $work/fast.txt; then
  echo "PASS: fast lines match classic lines ($(grep -c '^==' $work/fast.txt) commands)"
else
  echo "FAIL: fast lines differ from classic lines (above); evidence in $work" >&2; exit 1
fi
