#!/bin/bash
# Basic work in a line: init, status, diff, add, commit, log, show, rev-parse, rm, mv,
# restore, reset, clean, partial staging with apply --cached.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init demo --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/demo/lines/main || exit 1

eq "ignored files are not imported" "no" "$([ -e target/out.bin ] || [ -e debug.log ] && echo yes || echo no)"
eq "clean status" "nothing to save, working tree clean" "$(l status | tail -1)"
ln -s $LAYR_ROOT/projects/demo/lines/main $T/linked
eq "a link to a line works as the line" "main" "$(cd $T/linked && l line list --show-current)"
eq "porcelain clean" "" "$(l status --porcelain)"

sed -i 's/line 5/line five/' notes.txt; echo new > added.txt; rm src/main.rs; mkdir -p docs/deep; echo d > docs/deep/a.md
echo y > other.log; mkdir -p target; echo z > target/x
eq "short status" " M notes.txt
 D src/main.rs
?? added.txt
?? docs/" "$(l status -s)"
eq "untracked all" "?? docs/deep/a.md" "$(l status -s -uall | grep docs)"
eq "ignored listed with --ignored" "!! other.log
!! target/x" "$(l status -s --ignored | grep '^!!')"
eq "diff name-status" "M	notes.txt
D	src/main.rs" "$(l diff --name-status)"
eq "diff numstat" "1	1	notes.txt
0	3	src/main.rs" "$(l diff --numstat)"
has "diff patch hunk" "@@ -2,7 +2,7 @@ line 1" "$(l diff notes.txt)"
eq "diff exit code" "1" "$(l diff --quiet; echo $?)"
l add notes.txt docs
eq "staged and unstaged" "A  docs/deep/a.md
M  notes.txt
 D src/main.rs
?? added.txt" "$(l status -s)"
eq "diff --cached" "A	docs/deep/a.md
M	notes.txt" "$(l diff --cached --name-status)"
eq "pathspec without match" "128" "$(l add nothing.txt 2>/dev/null; echo $?)"
eq "ignored path needs -f" "1" "$(l add other.log 2>/dev/null; echo $?)"
has "commit output" "[main " "$(l save -m 'First change')"
eq "after commit" " D src/main.rs
?? added.txt" "$(l status -s)"
l save -qam "Remove main"
eq "log oneline" "Remove main
First change
initial state" "$(l log --format=%s)"
eq "log -n" "1" "$(l log --oneline -n 1 | wc -l)"
eq "log path filter" "First change" "$(l log --format=%s -- notes.txt | head -1)"
eq "show path" "line five" "$(l show head:notes.txt | sed -n 5p)"
eq "show dir" "tree head:docs" "$(l show head:docs | head -1)"
has "show stat" "notes.txt      | 2 +-" "$(l show --stat head~1)"
id=$(l rev-parse head)
eq "rev-parse full id length" "32" "${#id}"
eq "rev-parse short" "${id:0:7}" "$(l rev-parse --short head)"
eq "short id resolves" "$id" "$(l rev-parse ${id:0:9})"
eq "abbrev-ref" "main" "$(l rev-parse --abbrev-ref head)"
eq "toplevel" "$LAYR_ROOT/projects/demo/lines/main" "$(l rev-parse --show-toplevel)"
eq "HEAD~2 is the initial state" "initial state" "$(l log -1 --format=%s head~2)"
cd docs; eq "paths relative to the folder" "?? ../added.txt" "$(l status -s)"; cd ..

# rm and mv
l rm -q notes.txt
eq "rm stages a delete" "D  notes.txt" "$(l status -s | grep notes)"
l reset -q -- notes.txt; l restore notes.txt
eq "reset and restore bring it back" "" "$(l status -s | grep notes)"
l mv notes.txt renamed.txt
eq "mv is a staged move" "R  notes.txt -> renamed.txt" "$(l status -s | grep renamed)"
l save -qm "rename"
eq "commit of a move" "R100	notes.txt	renamed.txt" "$(l show --name-status --format= head)"

# restore and reset
echo junk >> renamed.txt
l restore renamed.txt
eq "restore drops the change" "" "$(l status -s | grep renamed)"
echo staged > s.txt; l add s.txt; l restore --staged s.txt
eq "restore --staged" "?? s.txt" "$(l status -s | grep s.txt)"
echo more >> renamed.txt; l save -qam "more"
l reset -q --soft head~1
eq "reset --soft keeps the change staged" "M  renamed.txt" "$(l status -s | grep renamed)"
l reset -q head
eq "reset --mixed unstages" " M renamed.txt" "$(l status -s | grep renamed)"
l reset -q --hard
eq "reset --hard drops tracked changes" "" "$(l status -s | grep renamed)"
eq "reset --hard keeps untracked files" "?? s.txt" "$(l status -s | grep s.txt)"

# clean
eq "clean dry run" "Would remove added.txt
Would remove s.txt" "$(l clean -n)"
l clean -fq
eq "clean removes untracked" "" "$(l status -s)"
eq "clean keeps ignored" "y" "$(cat other.log)"
eq "clean needs -f" "128" "$(l clean 2>/dev/null; echo $?)"

# partial staging: one hunk by patch
seq 1 40 | sed 's/^/row /' > big.txt; l add big.txt; l save -qm big
sed -i 's/^row 3$/ROW 3 staged/; s/^row 35$/ROW 35 not staged/' big.txt
l diff big.txt | awk '/^@@/{n++} n<2' > $T/hunk1.patch
l apply --cached $T/hunk1.patch
eq "partial add: index has the first hunk" "1	1	big.txt" "$(l diff --cached --numstat)"
eq "partial add: second hunk stays unstaged" "1	1	big.txt" "$(l diff --numstat)"
l save -qm "first hunk"
eq "commit has the first hunk only" "ROW 3 staged" "$(l show head:big.txt | grep -m1 ROW)"
has "working copy keeps the second hunk" "ROW 35 not staged" "$(cat big.txt)"

# commit without changes
eq "nothing to commit" "1" "$(l save -qm empty >/dev/null 2>&1; echo $?)"
eq "empty message is refused" "1" "$(l save -am '' >/dev/null 2>&1; echo $?)"
l save -q --allow-empty -m "empty"
eq "allow-empty" "empty" "$(l log -1 --format=%s)"
l save -q --amend -m "amended"
eq "amend replaces the head" "amended
first hunk" "$(l log -2 --format=%s)"

# symbolic links and modes
ln -s renamed.txt link; chmod +x big.txt; l add -A; l save -qm "link and mode"
eq "mode and link in name-status" "M	big.txt
A	link" "$(l show --name-status --format= head)"
eq "link target" "renamed.txt" "$(l show head:link)"
has "mode change shown" "new mode 100755" "$(l show --format= head)"
# clean -X removes ignored files only; a folder goes whole only when nothing in it is in the
# index and everything in it is of the kind being removed (review 2026-10-10).
mkdir -p mixed fresh; echo keep > mixed/keep.txt; echo add > mixed/added.txt; echo j > mixed/junk.log
echo keep > fresh/keep.txt; echo j > fresh/junk.log; mkdir -p target; echo z > target/x
l add mixed/added.txt
eq "clean -dX dry run" "Would remove fresh/junk.log
Would remove mixed/junk.log
Would remove other.log
Would remove target/" "$(l clean -ndX)"
l clean -fqdX
eq "clean -dX keeps untracked and added files" "keep add keep" "$(cat mixed/keep.txt mixed/added.txt fresh/keep.txt | xargs)"
eq "clean -dX removes the ignored ones" "" "$(ls -d mixed/junk.log fresh/junk.log other.log target 2>/dev/null)"
echo j > fresh/junk.log
eq "clean -d keeps ignored files of a new folder" "Would remove fresh/keep.txt
Would remove mixed/keep.txt" "$(l clean -nd)"
eq "clean -dx takes the whole new folder" "Would remove fresh/
Would remove mixed/keep.txt" "$(l clean -ndx)"
l clean -fqdx; l save -qm mixed
eq "clean -dx keeps the added file" "add" "$(cat mixed/added.txt)"

# stash --keep-index keeps the index; stash --all takes ignored files too.
echo staged >> big.txt; l add big.txt; echo unstaged >> renamed.txt
l park -q --keep-index
ok "the park revision resolves" l rev-parse 'park@{0}'
eq "stash --keep-index: the index stays" "M  big.txt" "$(l status -s)"
eq "stash --keep-index: the staged content stays" "staged" "$(tail -1 big.txt)"
l reset -q --hard
l park pop >/dev/null
eq "stash pop: both changes come back" " M big.txt
 M renamed.txt" "$(l status -s)"
l save -qam "before stash --all"
echo log > keep.log; echo u > untracked.txt; mkdir -p target; echo z > target/x
l park -q -u
eq "stash -u leaves ignored files" "log z no" "$(cat keep.log target/x | xargs) $([ -e untracked.txt ] && echo yes || echo no)"
l park pop >/dev/null
l park -q --all
eq "stash --all takes ignored files and folders" "" "$(ls -d keep.log untracked.txt target 2>/dev/null)"
l park pop >/dev/null
eq "stash pop brings ignored files back" "log u z" "$(cat keep.log untracked.txt target/x | xargs)"

printf 'message from a file\n' > "$T/message"
echo file-message >> big.txt
l add big.txt
ok "save -F creates a named state" l save -q -F "$T/message"
eq "save -F uses the file message" "message from a file" "$(l log -1 --format=%s)"
echo amendment >> big.txt
l add big.txt
ok "amend without a message creates a named state" l save -q --amend --no-edit
eq "amend without a message keeps the subject" "message from a file" "$(l log -1 --format=%s)"
eq "amend includes the staged bytes" amendment "$(l show head:big.txt | tail -1)"
echo long-file-option >> big.txt
ok "save --file accepts --all for tracked files" l save -q --all --file "$T/message"
eq "save --file includes tracked edits" long-file-option "$(l show head:big.txt | tail -1)"
finish
