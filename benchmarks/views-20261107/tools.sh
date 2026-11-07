#!/bin/zsh -f
# rg and find on a virtual line through the LayrFS tool server (`layr tool`), against a folder line.
#   LAYR_ROOT=$TMPDIR/layr-bench/store tools.sh <layr binary> <project> [rg pattern]
# Each call runs 3 times after one warm-up; the table shows the median in seconds.
set -u
L=${1:?layr binary}
PROJECT=${2:?project}
PATTERN=${3:-TODO}
: "${LAYR_ROOT:?set LAYR_ROOT to the test store}"
export LAYR_EXPERIMENTAL_VIEWS=1 LAYR_FS_SOCKET=$LAYR_ROOT/../fs-bench.sock LAYR_FS_STRICT=1
unset LAYR_FS_OFF LAYR_TOOL_ACTIVE RIPGREP_CONFIG_PATH
P=$LAYR_ROOT/projects/$PROJECT
cd $P/lines/main || exit 1
now() { perl -MTime::HiRes=time -e 'printf "%.4f", time' }
typeset -A R
t() { local k=$1 s e; shift; s=$(now); "$@" > /dev/null 2>&1 < /dev/null || echo "  (failed: $k)" >&2; e=$(now); R[$k]="${R[$k]:-} $(perl -e "printf '%.3f', $e-$s")"; }
med() { echo $* | tr ' ' '\n' | grep . | sort -n | sed -n 2p }
echo "load $(uptime | sed 's/.*averages: //')"
$L line new tnat > /dev/null
$L line new tvirt --view virtual > /dev/null
for kind in nat virt; do
    cd ../t$kind
    s=$(now); $L fs warm --wait > /dev/null; e=$(now)
    echo "index build t$kind: $(perl -e "printf '%.1f', $e-$s") s"
    cd $P/lines/main
done
cd ../tnat
for i in 0 1 2 3; do
    t rg-folder-real zsh -f -c "rg --sort path -l $PATTERN | wc -l"
    t rg-folder-tool zsh -f -c "$L tool rg -l $PATTERN | wc -l"
    t find-folder-real zsh -f -c "find . -type f | wc -l"
    t find-folder-tool zsh -f -c "$L tool find . -type f | wc -l"
done
cd ../tvirt
for i in 0 1 2 3; do
    t rg-virtual-tool zsh -f -c "$L tool rg -l $PATTERN | wc -l"
    t find-virtual-tool zsh -f -c "$L tool find . -type f | wc -l"
done
# An edit right before the call: a new file, then the query (the index takes the change first).
for i in 1 2 3; do
    echo "// $PATTERN bench $i" > bench-fresh-$i.txt
    t rg-virtual-after-edit zsh -f -c "$L tool rg -l $PATTERN | wc -l"
done
same() { cmp -s <(${=1} </dev/null) <(${=2} </dev/null) && echo same || echo DIFFERENT; }
echo "virtual rg -l: tool vs rg on the mount: $(same "$L tool rg -l $PATTERN" "rg --sort path -l $PATTERN")"
echo "virtual find: tool vs find on the mount (sorted): $(cmp -s <($L tool find . -type f | LC_ALL=C sort) <(find . -type f | LC_ALL=C sort) && echo same || echo DIFFERENT)"
for k in ${(k)R}; do [ $k = rg-virtual-after-edit ] || R[$k]=$(echo ${R[$k]} | awk '{$1=""; print}'); done  # drop the warm-up run
printf "%-28s %10s\n" call seconds
for k in rg-folder-real rg-folder-tool rg-virtual-tool rg-virtual-after-edit find-folder-real find-folder-tool find-virtual-tool; do
    printf "%-28s %10s\n" $k $(med ${R[$k]})
done
echo "raw:"; for k in ${(ko)R}; do echo "  $k: ${R[$k]}"; done
cd $P/lines/main
$L fs stop > /dev/null 2>&1
$L line delete -D tnat > /dev/null
$L line delete -D tvirt > /dev/null
echo "load $(uptime | sed 's/.*averages: //')"
