#!/bin/bash
# ci: macos overlayfs
# A merge frees the caches of the merged line (docs/cache-overlays.md, section 22): the merge takes them into the
# target, then the cache folder of the source line goes to the trash and its cache state goes too. A line with unsaved
# changes, a line with no save of its own, and main merged into a work line keep their caches. One user, so it runs
# on macOS and on Linux. tests/t105-follow.sh covers a squash merge on a remote.
# This fixture explicitly shares disposable output across build locations and retains its provenance.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable cache-portable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
TG=$(overlay target)
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
clear_cache_output target; mkdir -p target/debug; echo bin0 > target/debug/x
l save --overlays -q

# work <line>: a line from main with one saved change and a saved cache.
work() {
    l line new $1 main >/dev/null
    cd $P/$1
    echo $1 > $1.txt; l add $1.txt; l save -m "$1 work" -q
    clear_cache_output target; mkdir -p target/debug; echo bin-$1 > target/debug/x
    l save --overlays -q
    cd $P/main
}
# cache_dir <line>: the folder of the newest cache state of the line.
cache_dir() {
    local d
    d=$(cd $P/$1 && l overlay log $TG | sed -n 1p | cut -d' ' -f2)
    d="${d:20:12}${d:12:8}${d:0:12}"
    echo "$LAYR_ROOT/projects/p/meta/overlays/$TG/states/${d:0:8}-${d:8:4}-${d:12:4}-${d:16:4}-${d:20:12}"
}

# 1. A merged line: main takes its caches, then its cache folder and its cache state go.
work done
state=$(cache_dir done)
ok "the cache state of done is on the disk" test -d "$state"
out=$(l merge done 2>&1)
has "the merge takes the cache" "Merged cache target" "$out"
has "the merge frees the caches of the merged line" "Freed the caches of merged line 'done'" "$out"
eq "main has the cache of done" bin-done "$(cat target/debug/x)"
no "the cache folder of done is gone" test -e $P/done/target
no "the cache state of done is gone" test -e "$state"
has "done shows no saved cache" "not saved" "$(cd $P/done && l overlay list 2>&1)"
eq "done keeps its files" done "$(cat $P/done/done.txt)"
ok "fsck passes after the free" l fsck
has "no overlay data is left that nothing keeps" "0 unreachable state(s)" "$(l fsck --overlays 2>&1)"

# 2. A squash merge: the head of sq is no ancestor of main, but main holds its change. sq builds before its named
#    save, so its head names its cache state.
l line new sq main >/dev/null
cd $P/sq
clear_cache_output target; mkdir -p target/debug; echo bin-sq > target/debug/x
echo sq > sq.txt; l add sq.txt; l save -m "sq work" -q
state=$(cache_dir sq)
ok "the cache state of sq is on the disk" test -d "$state"
cd $P/main
out=$(l merge --squash sq -m "squash sq" 2>&1)
has "a squash merge frees the caches of the line" "Freed the caches of merged line 'sq'" "$out"
no "the cache folder of sq is gone" test -e $P/sq/target
no "the cache state that the head of sq names is gone" test -e "$state"

# 2b. A line whose saves add up to no change has no work in main: its caches stay.
work noop
cd $P/noop
l rm -q noop.txt; l save -m "drop noop" -q
cd $P/main
out=$(l merge --squash noop -m "squash noop" 2>&1)
no "a line with no net change is not freed" grep -q "Freed" <<<"$out"
eq "the cache of the line with no net change stays" bin-noop "$(cat $P/noop/target/debug/x)"

# 3. Unsaved changes keep the caches: a changed file, and a new file.
work dirty
work extra
echo more >> $P/dirty/dirty.txt
echo new > $P/extra/new.txt
out=$(l merge dirty 2>&1)
no "a line with a changed file is not freed" grep -q "Freed" <<<"$out"
eq "the cache of the line with a changed file stays" bin-dirty "$(cat $P/dirty/target/debug/x)"
out=$(l merge extra 2>&1)
no "a line with a new file is not freed" grep -q "Freed" <<<"$out"
eq "the cache of the line with a new file stays" bin-extra "$(cat $P/extra/target/debug/x)"

# 4. A line with no save of its own: a merge has nothing to take, and its build stays.
l line new fresh main >/dev/null
mkdir -p $P/fresh/target/debug; echo bin-fresh > $P/fresh/target/debug/x
l merge fresh >/dev/null 2>&1
eq "a line with no save of its own keeps its cache" bin-fresh "$(cat $P/fresh/target/debug/x)"

# 5. Main merged into a work line: main is not freed, and the work line is not merged into main.
work open
cd $P/open
out=$(l merge main 2>&1)
no "a merge of main into a work line frees nothing" grep -q "Freed" <<<"$out"
ok "main keeps its cache folder" test -f $P/main/target/debug/x
ok "the work line keeps its cache folder" test -f $P/open/target/debug/x

finish
