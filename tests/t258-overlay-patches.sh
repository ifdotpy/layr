#!/bin/bash
# ci: macos overlayfs
# Proprietary patches in an overlay (docs/overlays.md, section 9.3): the shared set is open source and
# `layr git push` publishes it. A private edit of a shared file, in a line that shows an overlay with
# `patches`, never reaches the shared history, the git remote or an automatic state. One user, so it
# runs on macOS and on Linux; t259 checks what another project reader can read.
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
ok "it may hold versions of shared files" l overlay set pro --patches
# Two more overlays for the refusals below: keys (no patches) in the view, other out of it.
l overlay create keys -q
l overlay create other -q
l overlay use keys pro >/dev/null 2>&1
has "the line shows it" "pro" "$(l overlay use)"
# A save that only changes overlays makes a state of the line with the same shared tree.
same_shared() { eq "$1" "" "$(l diff --name-only "$head0" head)"; }

# 1. The leak: a proprietary edit of a shared file and a new proprietary file, saved with -a.
echo "/* $MARK */" >> core.c
echo "int pro(void) { return 1; } /* $MARK */" > pro-feature.c
l add pro-feature.c
out=$(l save -am "pro work" 2>&1)
has "a save that would change shared files refuses and names --to shared" "--to shared" "$out"
has "and names --to <overlay>" "--to <overlay>" "$out"
no "the shared set has no proprietary file" l show head:pro-feature.c
eq "the shared head has no patch" 0 "$(l show head:core.c | grep -c "$MARK")"
l save -q --force
eq "the automatic state holds no patch" 0 "$(l show main@{0}:core.c 2>/dev/null | grep -c "$MARK")"
no "nor the proprietary file" l show main@{0}:pro-feature.c
l git push -q origin main >/dev/null 2>&1
eq "the git remote has no patch" 0 "$(leaks)"

# 2. save --to: the target of one named save (docs/overlays.md, section 9.3).
head0=$(l rev-parse head)
out=$(l save -q -am "pro work" --to nope 2>&1)
has "an unknown overlay is refused" "shows no overlay 'nope'" "$out"
out=$(l save -q -am "pro work" --to keys 2>&1)
has "a copy of a shared file into it is refused and names the property" "no property 'patches'" "$out"
has "is refused like an unknown one" "shows no overlay 'other'" "$(l save -q -am "pro work" --to other 2>&1)"
eq "no refused save made a state" "$head0" "$(l rev-parse head)"
ok "save --to pro" l save -q -am "pro work" --to pro
same_shared "the shared tree did not change"
has "the overlay has the patch" "$MARK" "$(l overlay show pro head:core.c)"
ok "and the proprietary file" l overlay show pro head:pro-feature.c
eq "the folder is clean" "" "$(l status --porcelain)"
eq "the folder has the patch" 1 "$(grep -c "$MARK" core.c)"
l git push -q origin main >/dev/null 2>&1
eq "the git remote has no patch after the save" 0 "$(leaks)"
no "--to needs a named save" l save -q --to pro
# --to shared: the shared set takes the change, also of a path that an overlay holds.
echo "/* fix */" >> lib.c
ok "save --to shared" l save -q -am "public fix" --to shared
has "the shared set has the fix" "fix" "$(l show head:lib.c)"
echo "int pub2;" >> pub.c
ok "a private copy of pub.c" l save -q -am "private pub" --to pro
ok "pub.c is in the overlay" l overlay show pro head:pub.c
echo "int pub3;" >> pub.c
ok "save --to shared publishes it" l save -q -am "publish pub" --to shared
has "the shared set has it" "pub3" "$(l show head:pub.c)"
no "the overlay copy is gone" l overlay show pro head:pub.c
eq "the folder is clean after it" "" "$(l status --porcelain)"
# rm --to: the target of a delete waits for the next named save.
ok "rm --to shared" l rm -q --to shared gone.c
ok "the delete goes to the shared set" l save -q -m "drop gone"
no "the shared set lost gone.c" l show head:gone.c
no "rm --to an unknown overlay" l rm -q --to nope keep.c

# 3. The line target (docs/overlays.md, section 9.3).
eq "a line has no target" none "$(l line target)"
no "a target must be an overlay that the line shows" l line target other
ok "set the target" l line target pro -q
eq "it is shown" pro "$(l line target)"
head0=$(l rev-parse head)
echo "/* $MARK second */" >> core.c
ok "a save without --to goes to the target" l save -q -am "second patch"
same_shared "the shared tree still did not change"
eq "the overlay has the second patch" 1 "$(l overlay show pro head:core.c | grep -c "$MARK second")"
echo "/* $MARK third */" >> core.c
echo "/* $MARK new */" > pro-two.c
l save -q --force
eq "the automatic state holds the shared head" "$(l show head:core.c)" "$(l show main@{0}:core.c)"
no "and leaves the new file out" l show main@{0}:pro-two.c
ok "reindex" l reindex -q
eq "reindex keeps the target" pro "$(l line target)"
ok "fsck is clean" l fsck
echo "int pub4;" >> pub.c
l add pub.c
ok "--to wins over the line target" l save -q -m "public pub" --to shared
has "the shared set has the public change" "pub4" "$(l show head:pub.c)"
eq "and no patch" 0 "$(l show head:core.c | grep -c "$MARK")"
rm -f pro-two.c
# A hidden overlay keeps the target in its own records: no record of the project names it.
ok "a hidden overlay with patches" l overlay create hiddenpatch7 --hidden --patches -q
l overlay use --add hiddenpatch7 >/dev/null 2>&1
ok "it can be the target" l line target hiddenpatch7 -q
eq "the target is shown" hiddenpatch7 "$(l line target)"
records() { python3 -c 'import sqlite3,sys; print("\n".join(r for (r,) in sqlite3.connect(sys.argv[1]).execute("SELECT raw FROM records")))' "$LAYR_ROOT/projects/p/meta/db/layr.sqlite"; }
eq "no record of the project names the hidden overlay" 0 "$(records | grep -c hiddenpatch7)"
ok "back to pro" l line target pro -q
eq "the hidden target is cleared" pro "$(l line target)"
# A line rename keeps the target (it is keyed by the line id).
ok "a second line" l line new w main -q
cd $LAYR_ROOT/projects/p/lines/w
ok "its target" l line target pro -q
ok "rename it" l line rename w w2
eq "the renamed line keeps its target" pro "$(l line target)"
ok "clear it" l line target --none -q
eq "it has none" none "$(l line target)"
cd $M

# 4. Copy on edit and private deletes (docs/overlays.md, section 9.6).
echo "/* $MARK lib */" >> lib.c
ok "an edit of a shared-only file" l save -q -am "patch lib"
eq "makes the overlay's copy" 1 "$(l overlay show pro head:lib.c | grep -c "$MARK lib")"
eq "the shared lib.c is unchanged" 0 "$(l show head:lib.c | grep -c "$MARK")"
rm core.c
ok "delete a patched file" l save -q -am "drop core"
ok "the shared set keeps core.c" l show head:core.c
no "the folder lacks it" test -e core.c
eq "and status is clean" "" "$(l status --porcelain)"
eq "the overlay hides it" core.c "$(l overlay show pro head:.layrremoved)"
no "the overlay copy is gone" l overlay show pro head:core.c
rm util.c
ok "delete a shared-only file" l save -q -am "drop util"
ok "the shared set keeps util.c" l show head:util.c
eq "the marker lists both" "core.c util.c" "$(l overlay show pro head:.layrremoved | tr '\n' ' ' | sed 's/ $//')"
ok "a line without the overlay" l line new pub main -q
cd $LAYR_ROOT/projects/p/lines/pub
l overlay use --none >/dev/null 2>&1
eq "other readers see the shared files" "int core(void) { return 0; }" "$(head -1 core.c)"
ok "and util.c" test -e util.c
cd $M
echo "int util2;" > util.c
l add util.c
ok "a file at a hidden path comes back" l save -q -am "util again"
eq "in the overlay" "int util2;" "$(l overlay show pro head:util.c)"
eq "and the marker drops it" core.c "$(l overlay show pro head:.layrremoved)"
eq "the folder is clean after it" "" "$(l status --porcelain)"
ok "layr mv of a shared file" l mv moved.c moved2.c
ok "a save of the move" l save -q -m "move it"
ok "the shared set keeps moved.c" l show head:moved.c
no "and has no moved2.c" l show head:moved2.c
ok "the overlay has moved2.c" l overlay show pro head:moved2.c
has "and hides moved.c" moved.c "$(l overlay show pro head:.layrremoved)"
# P1 for deletes: an overlay without patches never hides a shared file.
ok "rm --to keys" l rm -q --to keys keep.c
has "the save refuses and names the property" "no property 'patches'" "$(l save -q -m "hide keep" 2>&1)"
ok "reset brings keep.c back" l reset -q --hard
ok "keep.c is back" test -e keep.c
# overlay move keeps its meaning: the shared set loses the file.
l overlay move pro keep.c >/dev/null
ok "a save after overlay move" l save -q -m "move keep"
no "the shared set lost keep.c" l show head:keep.c
ok "the overlay has it" l overlay show pro head:keep.c
# The marker is a file of the overlay's tree: its git remote carries it.
git init -q --bare -b main $T/pro.git
ok "an overlay git remote" l overlay remote add pro pro-gh $T/pro.git
ok "push every layer" l push -q
has "the overlay remote has the markers" "core.c" "$(git --git-dir=$T/pro.git show main:.layrremoved)"
eq "the shared git remote has no private patch" 0 "$(leaks)"
eq "nor the copy of lib.c" 0 "$(git --git-dir=$T/r.git log --all -p | grep -c "$MARK lib")"

finish
