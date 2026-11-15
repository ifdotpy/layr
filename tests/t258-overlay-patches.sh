#!/bin/bash
# ci: macos overlayfs
# Proprietary patches in an overlay (docs/overlays.md section 1, write rule, owner decision 2026-11-15):
# `main` is open source and `layr git push` publishes it. A named save in a view of an overlay writes
# every change to that overlay: a private edit of a file of `main`, a private new file and a private
# delete never change `main`, never reach the git remote and never an automatic state. Only
# `save --to main` changes `main`. One user, so it runs on macOS and on Linux; t259 checks what another
# project reader can read.
. "$(dirname "$0")/lib.sh"

MARK=proprietary-91c4
mkdir -p $T/src
printf 'int core(void) { return 0; }\n' > $T/src/core.c
printf 'int lib(void) { return 0; }\n' > $T/src/lib.c
for f in util gone keep moved pub; do printf 'int %s(void) { return 0; }\n' $f > $T/src/$f.c; done
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines/main
cd $M
git init -q --bare -b main $T/r.git
l git remote add origin $T/r.git >/dev/null
ok "push the open-source start" l git push -q origin main
leaks() { git --git-dir=$T/r.git log --all -p | grep -c "$MARK"; }
ok "an overlay for proprietary patches" l overlay create pro -q
l overlay create other -q
ok "the folder of main shows pro" l overlay use pro -q
has "the line shows it" "pro" "$(l overlay use)"
head0=$(l rev-parse head)
# The delete markers of pro: its metadata (docs/overlays.md section 1), which no folder shows.
hidden() { l overlay show pro head:.layrmeta/meta.json 2>/dev/null | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin).get("removed",[])))'; }
same_main() { eq "$1" "" "$(l diff --name-only "$head0" head)"; }

# 1. The leak of the old rules: a private edit of a file of main and a new private file, saved with -a.
echo "/* $MARK */" >> core.c
echo "int pro(void) { return 1; } /* $MARK */" > pro-feature.c
l add pro-feature.c
ok "a named save in the view of pro" l save -q -am "pro work"
same_main "main did not change"
no "main has no proprietary file" l show head:pro-feature.c
eq "main has no patch" 0 "$(l show head:core.c | grep -c "$MARK")"
has "pro has the patch" "$MARK" "$(l overlay show pro head:core.c)"
ok "and the proprietary file" l overlay show pro head:pro-feature.c
eq "the folder is clean" "" "$(l status --porcelain)"
eq "the folder has the patch" 1 "$(grep -c "$MARK" core.c)"
echo "/* $MARK auto */" >> core.c
l save -q --force
eq "the automatic state holds no patch" 0 "$(l show main@{0}:core.c 2>/dev/null | grep -c "$MARK")"
no "nor the proprietary file" l show main@{0}:pro-feature.c
ok "the patch goes to pro" l save -q -am "pro work 2"
l git push -q origin main >/dev/null 2>&1
eq "the git remote has no patch" 0 "$(leaks)"

# 2. A private delete: pro hides the file, main keeps it.
rm util.c
ok "a save of a delete" l save -q -am "drop util"
ok "main keeps util.c" l show head:util.c
no "the folder lacks it" test -e util.c
eq "status is clean" "" "$(l status --porcelain)"
eq "pro hides it" util.c "$(hidden)"
no "the folder has no metadata" test -e .layrmeta
ok "a line from here, then the view of main" l line new pub main -q
cd $LAYR_ROOT/projects/p/lines/pub
ok "pub shows main" l overlay use main -q
eq "other readers see the files of main" "int core(void) { return 0; }" "$(head -1 core.c)"
ok "and util.c" test -e util.c
cd $M
echo "int util2;" > util.c
l add util.c
ok "a file at a hidden path comes back" l save -q -am "util again"
eq "in pro" "int util2;" "$(l overlay show pro head:util.c)"
eq "and the marker is gone" "" "$(hidden)"
ok "layr mv of a file of main" l mv moved.c moved2.c
ok "a save of the move" l save -q -m "move it"
ok "main keeps moved.c" l show head:moved.c
no "and has no moved2.c" l show head:moved2.c
ok "pro has moved2.c" l overlay show pro head:moved2.c
has "and hides moved.c" moved.c "$(hidden)"
same_main "main still did not change"
l git push -q origin main >/dev/null 2>&1
eq "the git remote has no patch after the deletes" 0 "$(leaks)"

# 3. save --to main: the changes of one save go to main (docs/overlays.md section 1).
echo "/* fix */" >> lib.c
ok "save --to main" l save -q -am "public fix" --to main
has "main has the fix" "fix" "$(l show head:lib.c)"
no "pro has no copy of lib.c" l overlay show pro head:lib.c
eq "the folder is clean after it" "" "$(l status --porcelain)"
# A patch of pro saved with --to main: main takes it, and pro drops its copy.
echo "int pub2;" >> pub.c
ok "a private copy of pub.c" l save -q -am "private pub"
ok "pub.c is in pro" l overlay show pro head:pub.c
echo "int pub3;" >> pub.c
ok "save --to main publishes it" l save -q -am "publish pub" --to main
has "main has it" "pub3" "$(l show head:pub.c)"
no "the copy of pro is gone" l overlay show pro head:pub.c
# Mixed work is two saves: stage the public part and save it with --to main, then save the rest.
echo "int lib2;" >> lib.c
echo "/* $MARK mixed */" >> core.c
l add lib.c
ok "the staged public part goes to main" l save -q -m "public lib" --to main
has "main has it" "lib2" "$(l show head:lib.c)"
eq "main has no patch" 0 "$(l show head:core.c | grep -c "$MARK")"
eq "the private part waits unstaged" " M core.c" "$(l status --porcelain)"
ok "the rest goes to pro" l save -q -am "private core"
eq "pro has it" 1 "$(l overlay show pro head:core.c | grep -c "$MARK mixed")"
eq "main still has no patch" 0 "$(l show head:core.c | grep -c "$MARK")"
l git push -q origin main >/dev/null 2>&1
eq "the git remote has the public fixes" 1 "$(git --git-dir=$T/r.git show main:lib.c | grep -c lib2)"
eq "and no patch" 0 "$(leaks)"

# 4. Refusals: --to names main or an overlay of the path of the view.
head1=$(l rev-parse head)
echo "/* $MARK refused */" >> core.c
has "an unknown overlay is refused" "is not on the path of the view" "$(l save -q -am "x" --to nope 2>&1)"
has "an overlay off the path is refused the same way" "overlay 'other' is not on the path of the view" "$(l save -q -am "x" --to other 2>&1)"
has "the old name shared is refused" "is not on the path of the view" "$(l save -q -am "x" --to shared 2>&1)"
eq "--to needs a named save (exit 2)" 2 "$(l save -q --to pro >/dev/null 2>&1; echo $?)"
eq "save with a path fails (exit 128)" 128 "$(l save -q -m "x" core.c >/dev/null 2>&1; echo $?)"
has "and names layr add" "layr add" "$(l save -q -m "x" core.c 2>&1)"
eq "no refused save made a state" "$head1" "$(l rev-parse head)"
eq "and pro has no refused patch" 0 "$(l overlay show pro head:core.c | grep -c "$MARK refused")"
eq "line target was removed (exit 2)" 2 "$(l line target pro >/dev/null 2>&1; echo $?)"
eq "rm --to was removed (exit 2)" 2 "$(l rm -q --to main gone.c >/dev/null 2>&1; echo $?)"
eq "--patches was removed (exit 2)" 2 "$(l overlay set pro --patches >/dev/null 2>&1; echo $?)"
ok "the refused patch goes to pro" l save -q -am "refused patch saved"

# 5. A tree: deep above pro. A save in the view of deep writes deep; --to pro writes pro.
ok "an overlay above pro" l overlay create deep --parent pro -q
ok "the folder shows deep" l overlay use deep -q
eq "the view is the path" "pro deep" "$(l overlay use | cut -f1 | paste -sd' ' -)"
echo "/* $MARK deep */" >> core.c
ok "a save in the view of deep" l save -q -am "deep patch"
eq "deep has the patch" 1 "$(l overlay show deep head:core.c | grep -c "$MARK deep")"
eq "pro does not" 0 "$(l overlay show pro head:core.c | grep -c "$MARK deep")"
echo "/* $MARK to pro */" >> core.c
ok "save --to pro" l save -q -am "to pro" --to pro
eq "pro has it" 1 "$(l overlay show pro head:core.c | grep -c "$MARK to pro")"
no "deep dropped its copy" l overlay show deep head:core.c
eq "the folder is clean" "" "$(l status --porcelain)"
# overlay move along the path (docs/overlays.md section 1): a file of deep moves to pro, and deep drops it.
echo "/* $MARK move */" > d.c
l add d.c
ok "a file of deep" l save -q -m "d"
ok "deep has it" l overlay show deep head:d.c
has "a move off the path of the view is refused" "is not on the path of the view" "$(l overlay move other d.c 2>&1)"
ok "overlay move to pro" l overlay move -q pro d.c
ok "the save of the move" l save -q -m "d to pro"
ok "pro has d.c" l overlay show pro head:d.c
no "deep dropped it" l overlay show deep head:d.c
no "main never had it" l show head:d.c
# A patch of a file of main moved from deep to pro: pro takes the version of the view, deep drops it, and
# main keeps its own version.
echo "/* $MARK patch move */" >> lib.c
ok "a patch of lib.c in deep" l save -q -am "deep lib"
ok "overlay move of the patch to pro" l overlay move -q pro lib.c
ok "the save of the patch move" l save -q -m "lib to pro"
eq "pro has the patch" 1 "$(l overlay show pro head:lib.c | grep -c "$MARK patch move")"
no "deep dropped it" l overlay show deep head:lib.c
ok "main keeps lib.c" l show head:lib.c
eq "main has no patch" 0 "$(l show head:lib.c | grep -c "$MARK")"
has "pro records it as a patch" "lib.c" "$(l overlay show pro --json | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)["paths"]["patches"]))')"
eq "the folder is clean" "" "$(l status --porcelain)"
same_lib() { l show head:lib.c; }
has "main is as before" "lib2" "$(same_lib)"
eq "and has no patch" 0 "$(l show head:core.c | grep -c "$MARK")"
# A hidden overlay above deep: no record of the project names it.
ok "a hidden overlay above deep" l overlay create hiddenpatch7 --parent deep --hidden -q
ok "the folder shows it" l overlay use hiddenpatch7 -q
echo "/* $MARK hidden */" > hidden.c
l add hidden.c
ok "a save in its view" l save -q -m "hidden file"
ok "the hidden overlay has the file" l overlay show hiddenpatch7 head:hidden.c
no "main does not" l show head:hidden.c
records() { python3 -c 'import sqlite3,sys; print("\n".join(r for (r,) in sqlite3.connect(sys.argv[1]).execute("SELECT raw FROM records")))' "$LAYR_ROOT/projects/p/meta/db/layr.sqlite"; }
eq "no record of the project names the hidden overlay" 0 "$(records | grep -c hiddenpatch7)"
ok "back to the view of pro" l overlay use pro -q
no "hidden.c left the folder" test -e hidden.c

# 6. overlay move keeps its meaning: the file leaves main at the next save, and the output says so.
out=$(l overlay move pro keep.c)
has "move says that main loses the file" "keep.c' is in main: the next save deletes it from main" "$out"
has "and that the next git push publishes the delete" "the next 'layr git push' publishes that delete" "$out"
ok "a save after overlay move" l save -q -m "move keep"
no "main lost keep.c" l show head:keep.c
ok "pro has it" l overlay show pro head:keep.c
# The markers are a file of the overlay's tree: its git remote carries them.
git init -q --bare -b main $T/pro.git
ok "an overlay git remote" l overlay remote add pro pro-gh $T/pro.git
ok "push every layer" l push -q
has "the overlay remote has the markers" "moved.c" "$(git --git-dir=$T/pro.git show main:.layrmeta/meta.json)"
eq "the git remote of main has no private patch" 0 "$(leaks)"
ok "fsck is clean" l fsck
ok "reindex" l reindex -q
eq "the view survives reindex" "pro" "$(l overlay use | cut -f1 | paste -sd' ' -)"

finish
