#!/bin/bash
# E8: layr itself on Linux: btrfs folder lines against LayrFS lines (hub), same store and project.
set -euo pipefail
export LAYR=/home/layrfs-it/layr LAYR_ROOT=/home/layrfs-it/store LAYR_SOCKET=/home/layrfs-it/run/layr.sock
export LAYR_EXPERIMENTAL_VIEWS=1 LAYR_AUTHOR_NAME=Lab LAYR_AUTHOR_EMAIL=lab@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export PATH=$HOME/.local/bin:$PATH TMPDIR=$HOME/lfx/lab/tmp
SRC=${SRC:-/home/layrfs-src/layr}
p=${P:-e8-$(basename $SRC)-$$}
now() { date +%s%N; }
ms() { echo $(( ($2 - $1) / 1000000 )); }
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
if [ -z "${P:-}" ]; then
t0=$(now); $LAYR init $p --from $SRC -q >/dev/null; t1=$(now); echo "init $(ms $t0 $t1) ms"
cd $LAYR_ROOT/projects/$p/lines/main
$LAYR protect main --remove >/dev/null
t0=$(now); $LAYR migrate --delete-old >/dev/null; t1=$(now); echo "migrate $(ms $t0 $t1) ms"
fi
cd $LAYR_ROOT/projects/$p/lines/main
files=$(cd $SRC && find . -type f -not -path './.git/*' -size +0 -printf '%P\n' | sort | head -10 || true)
run() { # run <label> <line new args...>
  local label=$1 c=() d=() s=() i n; shift
  for i in 1 2 3 4 5; do
    n=$label$i
    t0=$(now); $LAYR line new $n "$@" -q >/dev/null; t1=$(now); c+=($(ms $t0 $t1))
    P=$LAYR_ROOT/projects/$p/lines/$n
    for f in $files; do printf '// e8\n' >> "$P/$f"; done
    t0=$(now); (cd $P && $LAYR save -a -m e8 -q 2>/dev/null || $LAYR save -m e8 -q); t1=$(now); s+=($(ms $t0 $t1))
    t0=$(now); $LAYR line delete -D $n >/dev/null; t1=$(now); d+=($(ms $t0 $t1))
  done
  printf '%-10s line new %4s ms  save 10 %4s ms  line delete %4s ms   (new: %s)\n' $label \
    "$(printf '%s\n' "${c[@]}" | median)" "$(printf '%s\n' "${s[@]}" | median)" "$(printf '%s\n' "${d[@]}" | median)" "${c[*]}"
}
run folder
LAYR_VIEW_BACKEND= run vbtrfs --view virtual
LAYR_VIEW_BACKEND=layrfs run layrfs --view virtual
