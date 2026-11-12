#!/bin/bash
# ci: macos overlayfs
# A new line takes a warm cache as one copy. Writes into that copy leave the source cache and the
# parent line unchanged. On the non-OverlayFS builds the cache folder is taken once (APFS: one clone).
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd $P/main || exit 1
l config regenerable '["target"]'
# Hard links, as cargo makes them in target/debug.
rm -rf target; mkdir -p target/debug; echo v1 > target/debug/x; ln target/debug/x target/debug/x-link
l save --overlays --folder target -q >/dev/null 2>&1
cache_id() { l overlay list | grep -o 'saved [0-9a-f]*'; }
saved=$(cache_id)
eq "main saved its cache" 1 "$(printf '%s\n' "$saved" | grep -c saved)"

start=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
LAYR_TRACE=1 l line new w >/dev/null 2>"$T/new.trace"
rc=$?
end=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
printf 'timing: line new w with a warm cache took %s ms\n' "$(perl -e "printf '%.0f', ($end - $start) * 1000")" >&2
eq "line new succeeds" 0 "$rc"

if [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    eq "the cache folder is copied once" 1 "$(grep -cE '(APFS clone|snapshot) .*/target$' "$T/new.trace")"
fi

cd $P/w || exit 1
eq "the line gets the cache" v1 "$(cat target/debug/x)"
eq "the hard-linked name has its data" v1 "$(cat target/debug/x-link)"
eq "every entry of the copy has the owner of its root" \
    "$(ls -ldn target | awk '{print $3":"$4}')" "$(ls -ln target/debug/x | awk '{print $3":"$4}')"
echo changed > target/debug/x
echo new > target/debug/added
eq "the parent line keeps its cache" v1 "$(cat $P/main/target/debug/x)"
eq "the parent line gets no new file" "" "$(ls $P/main/target/debug/added 2>/dev/null)"
eq "the source cache state is unchanged" "$saved" "$(cache_id)"

l line new -q w2 >/dev/null
eq "a second line from the same start takes the unchanged cache" v1 "$(cat $P/w2/target/debug/x)"
eq "a second line gets no file of the first" "" "$(ls $P/w2/target/debug/added 2>/dev/null)"
eq "the cache state stays after the second take" "$saved" "$(cache_id)"
finish
