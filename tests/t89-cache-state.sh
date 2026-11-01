#!/bin/bash
# A cache folder (regenerable, ignored) stays out of the states of its line: neither the save that
# makes it a cache, nor an automatic save after a commit. On APFS a new folder is copied with one
# clone, so a cache that is a nested layer must not come along with its content.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf 'target regenerable\n' > $T/src/.layrattributes
l init c --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/c/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q w >/dev/null
cd $P/w || exit 1

# The build output, then the save that makes it a cache.
rm -rf target; mkdir -p target/debug; echo one > target/debug/x
l save --caches -q >/dev/null
s1=$(l op log | grep -m1 ' save ' | awk '{print $7}')
has "the save --caches state exists" "tree" "$(l show "$s1:target" 2>&1)"
eq "the save --caches state holds no cache file" 0 "$(l show "$s1:target" | grep -c debug)"
eq "the cache keeps its file" one "$(cat target/debug/x)"

# A commit moves the head past the cache state. The next automatic save must not copy the cache.
echo two >> notes.txt
l add notes.txt
l save -qm "commit" >/dev/null
echo two > target/debug/x
echo more >> src/main.rs
l reset >/dev/null
s2=$(l op log | grep -m1 'before a layr command' | awk '{print $7}')
has "the automatic save state exists" "tree" "$(l show "$s2:target" 2>&1)"
eq "the automatic save after a commit holds no cache file" 0 "$(l show "$s2:target" | grep -c debug)"
eq "the cache keeps its new file" two "$(cat target/debug/x)"
finish
