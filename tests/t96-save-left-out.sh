#!/bin/bash
# A named save (save -m, with or without -a) names on stderr the files it left out: untracked
# files (a new folder as dir/) and tracked files whose working content differs from the new state.
# An ignored file, a cache folder and a clean save name nothing; more than 20 paths end in "and N more".
. "$(dirname "$0")/lib.sh"

sample $T/src
printf 'target regenerable\n' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q w >/dev/null
cd $P/w || exit 1
err=$T/save-err.txt

# The build output of the ignored cache folder, saved as a cache.
rm -rf target; mkdir -p target; echo bin > target/out.bin; echo x > debug.log
l save --caches -q >/dev/null

# A clean save: a modified file, added and saved; nothing is left out.
echo more >> notes.txt; l add notes.txt >/dev/null
l save -m clean 2> $err >/dev/null
eq "a save that leaves nothing out prints nothing" "" "$(cat $err)"

# A new file and a modified file that is not added; only notes.txt is added.
echo new > added.txt; echo more >> src/main.rs
echo again >> notes.txt; l add notes.txt >/dev/null
l save -m one 2> $err >/dev/null
eq "save -m names the untracked file" "not saved (untracked): added.txt" "$(grep '^not saved (untracked)' $err)"
eq "save -m names the modified file that is not added" "not saved (modified, not added): src/main.rs" "$(grep '^not saved (modified' $err)"
eq "the warning has two lines only" 2 "$(wc -l < $err | tr -d ' ')"
eq "the ignored file and the cache folder are not named" "" "$(grep -e debug.log -e target $err)"

# save -a saves the modified file, so only the untracked file is left out.
l save -a -m two 2> $err >/dev/null
eq "save -a names only the untracked file" "not saved (untracked): added.txt" "$(cat $err)"

# More than 20 untracked files: the first 20 are named, then the count of the others.
rm -f added.txt
for i in $(seq -w 1 25); do echo x > f$i.txt; done
echo x >> notes.txt; l add notes.txt >/dev/null
l save -m three 2> $err >/dev/null
first=$(for i in $(seq -w 1 20); do printf 'f%s.txt, ' $i; done | sed 's/, $//')
eq "more than 20 paths end in and N more" "not saved (untracked): $first, and 5 more" "$(cat $err)"
rm -f f[0-9][0-9].txt

# A new folder that is wholly untracked is named once, as dir/.
mkdir -p newdir; echo a > newdir/a.txt; echo b > newdir/b.txt
echo y >> notes.txt; l add notes.txt >/dev/null
l save -m four 2> $err >/dev/null
eq "a new folder is named as dir/" "not saved (untracked): newdir/" "$(cat $err)"
finish
