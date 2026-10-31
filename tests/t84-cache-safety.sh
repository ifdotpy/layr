#!/bin/bash
# A folder becomes a cache by a copy (save --caches, overlay repair). The copy never deletes a
# folder it did not make, and never copies a name that is not UTF-8 wrongly.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd $P/main || exit 1
l config regenerable '["target"]'
l line new -q w >/dev/null
cd $P/w || exit 1

# A user backup next to the cache folder, under the names an old build used for temporaries.
rm -rf target; mkdir target; echo cache > target/x
mkdir target.layr-old target.layr-new
echo old-backup > target.layr-old/precious
echo new-backup > target.layr-new/precious
l save --caches --cache target -q >/dev/null 2>&1
eq "the cache keeps its file" cache "$(cat target/x)"
eq "a foreign target.layr-old is kept" old-backup "$(cat target.layr-old/precious)"
eq "a foreign target.layr-new is kept" new-backup "$(cat target.layr-new/precious)"
rm -rf target; mkdir target; echo cache2 > target/y
l overlay repair cache.target >/dev/null 2>&1
eq "overlay repair keeps the new cache file" cache2 "$(cat target/y)"
eq "overlay repair keeps a foreign target.layr-old" old-backup "$(cat target.layr-old/precious)"
eq "overlay repair keeps a foreign target.layr-new" new-backup "$(cat target.layr-new/precious)"
eq "no temporary folder of ours stays" "target.layr-new target.layr-old" "$(ls -d target.layr-* | tr '\n' ' ' | sed 's/ $//')"

# Names that are not UTF-8: a lossy name would map two files to one string and lose one.
if [ "$(uname -s)" = Darwin ]; then
    : # APFS cannot create such names
else
    rm -rf target target.layr-*; mkdir target
    printf 'one\n' > "target/bad$(printf '\xff')"
    printf 'two\n' > "target/bad$(printf '\xef\xbf\xbd')"
    l save --caches --cache target -q >/dev/null 2>&1
    eq "both odd names remain" 2 "$(ls target | wc -l | tr -d ' ')"
    eq "the 0xff name keeps its data" one "$(cat "target/bad$(printf '\xff')")"
    eq "the U+FFFD name keeps its data" two "$(cat "target/bad$(printf '\xef\xbf\xbd')")"
    eq "no temporary folder stays" "" "$(ls -d target.layr-* 2>/dev/null)"
fi
finish
