#!/bin/zsh
# Virtual line (LayrFS mount) against a folder line (APFS clone) on one project. docs/views.md.
# Needs the LayrFS extension (tools/layrfs/README.md) and a project in a test store:
#   LAYR_ROOT=$TMPDIR/layr-bench/store bench.sh <layr binary> <project> [rg pattern]
# Each step runs 3 times; the table shows the median in seconds.
set -u
L=${1:?layr binary}
PROJECT=${2:?project}
PATTERN=${3:-TODO}
export LAYR_EXPERIMENTAL_VIEWS=1
: "${LAYR_ROOT:?set LAYR_ROOT to the test store}"
P=$LAYR_ROOT/projects/$PROJECT
cd $P/lines/main || exit 1
now() { perl -MTime::HiRes=time -e 'printf "%.4f", time' }
typeset -A R
# t <key> <command...>: run once in the current folder and record the wall time.
t() { local k=$1 s e; shift; s=$(now); "$@" > /dev/null 2>&1 < /dev/null || echo "  (failed: $k: $*)" >&2; e=$(now); R[$k]="${R[$k]:-} $(perl -e "printf '%.3f', $e-$s")"; }
med() { echo $* | tr ' ' '\n' | grep . | sort -n | sed -n 2p }
# 10 edits: 6 changed files, 2 new files, 1 delete, 1 rename. Round $2 changes different content.
FILES=(README.md BUILD.gn DEPS base/BUILD.gn chrome/BUILD.gn content/BUILD.gn net/BUILD.gn ui/BUILD.gn)
edits() {
    local d=$1 r=$2
    for f in $FILES[1,6]; do echo "// bench $r" >> $d/$f; done
    echo "new $r" > $d/bench-new-$r-a.txt
    echo "new $r" > $d/base/bench-new-$r-b.txt
    rm $d/$FILES[7]
    mv $d/$FILES[8] $d/$FILES[8].moved-$r
}
echo "project $PROJECT: $(find $P/lines/main -type f | wc -l | tr -d ' ') files; load $(uptime | sed 's/.*averages: //')"
for i in 1 2 3; do
    t new-folder $L line new nat$i
    t new-virtual $L line new virt$i --view virtual
done
# Reads: warm once, then 3 runs each.
for kind in nat virt; do
    (cd ../${kind}1 && rg -l $PATTERN > /dev/null 2>&1; find . -type f > /dev/null 2>&1)
done
for i in 1 2 3; do
    for kind in nat virt; do
        cd ../${kind}1
        t rg-$kind zsh -c "rg -l $PATTERN | wc -l"
        t find-$kind zsh -c "find . -type f | wc -l"
        cd $P/lines/main
    done
done
echo "rg -l $PATTERN: folder $(cd ../nat1 && rg -l $PATTERN | wc -l | tr -d ' ') files, virtual $(cd ../virt1 && rg -l $PATTERN | wc -l | tr -d ' ') files"
echo "find -type f: folder $(cd ../nat1 && find . -type f -not -path './.git/*' | wc -l | tr -d ' '), virtual $(cd ../virt1 && find . -type f | wc -l | tr -d ' ')"
# Status after 10 edits, and save of them: one round in each line.
for i in 1 2 3; do
    for kind in nat virt; do
        cd ../$kind$i
        $L status > /dev/null 2>&1   # the first status of a new folder line builds its status cache
        edits . $i
        t status-$kind $L status
        if [ $kind = nat ]; then
            $L add bench-new-$i-a.txt base/bench-new-$i-b.txt $FILES[8].moved-$i > /dev/null 2>&1
            t save-$kind $L save -a -m "bench $i"
        else
            t save-$kind $L save -m "bench $i"
        fi
        cd $P/lines/main
    done
done
echo "states equal: $($L diff --exit-code nat1 virt1 > /dev/null && echo yes || echo NO)"
for i in 1 2 3; do
    t delete-folder $L line delete -D nat$i
    t delete-virtual $L line delete -D virt$i
done
# A thin start state: the first virtual line makes its complete tree once; the next one mounts at once.
echo "// thin" >> README.md && $L save -a -m "thin start" > /dev/null
thin=$($L rev-parse main)
s=$(now); $L line new thin1 $thin --view virtual > /dev/null; e=$(now); first=$(perl -e "printf '%.3f', $e-$s")
s=$(now); $L line new thin2 $thin --view virtual > /dev/null; e=$(now); second=$(perl -e "printf '%.3f', $e-$s")
$L line delete -D thin1 > /dev/null; $L line delete -D thin2 > /dev/null
echo "virtual line on a thin state: first $first s (makes the complete tree), second $second s"
printf "%-24s %10s %10s\n" step folder virtual
for s in new status save delete; do
    case $s in new|delete) a=$s-folder; b=$s-virtual ;; *) a=$s-nat; b=$s-virt ;; esac
    printf "%-24s %10s %10s\n" "line $s" $(med ${R[$a]}) $(med ${R[$b]})
done
printf "%-24s %10s %10s\n" "rg -l $PATTERN" $(med ${R[rg-nat]}) $(med ${R[rg-virt]})
printf "%-24s %10s %10s\n" "find -type f" $(med ${R[find-nat]}) $(med ${R[find-virt]})
echo "raw:"; for k in ${(ko)R}; do echo "  $k:${R[$k]}"; done
echo "load $(uptime | sed 's/.*averages: //')"
