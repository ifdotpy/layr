#!/bin/bash
# ci: macos overlayfs
# A patch is a patch (docs/overlays.md section 1, owner decision 2026-11-15): an overlay records, for each
# patched path, the version of the parent that the patch was made from. When the parent changes that path,
# the patch merges with it (clean: a new version without a question; an overlap: conflict markers; a parent
# that deleted the path: a conflict of kind parent). An overlay keeps a path only when it differs from the
# view of its parent, and `restore --parent` puts the parent's version in the folder. The metadata is a
# versioned folder of the overlay state that no folder shows, and a merge of the overlay merges it by
# structure. One user.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src
seq 1 10 > $T/src/lib.c
for f in a b x y; do echo "$f" > $T/src/$f.c; done
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines/main
cd $M
l protect main --remove >/dev/null 2>&1 || true
meta() { l overlay show pro head:.layrmeta/meta.json 2>/dev/null; }
field() { python3 -c 'import json,sys; d=json.load(sys.stdin); v=d.get(sys.argv[1], {}); print(" ".join(sorted(v)))' "$1"; }
paths() { l overlay show pro --json | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)["paths"][sys.argv[1]]))' "$1"; }
ok "an overlay pro" l overlay create pro -q
ok "the folder of main shows pro" l overlay use pro -q

# 1. A patch records its base.
sed_inplace -e 's/^10$/ten-pro/' lib.c
out=$(l save -am "patch lib")
has "the save says which paths are patches" "pro: patches of the parent: lib.c" "$out"
eq "the metadata names the base of lib.c" lib.c "$(meta | field bases)"
eq "overlay show --json lists the patch" lib.c "$(paths patches)"
eq "and nothing waits" "" "$(paths waiting)"
has "status says where named saves go" "Named saves go to overlay pro" "$(l status)"
has "and lists the patch" "pro: patches of the parent: lib.c" "$(l status)"
no "the folder never shows the metadata" test -e .layrmeta
echo new > new.c
l add new.c
ok "a new file" l save -qm "new file"
eq "is no patch" new.c "$(paths own)"
mkdir -p .layrmeta && echo x > .layrmeta/x
l add .layrmeta/x >/dev/null 2>&1
has "a file at the metadata path is refused" "holds the metadata of an overlay" "$(l save -qam "meta file" 2>&1)"
rm -rf .layrmeta; l reset -q >/dev/null 2>&1

# 2. The parent changes the path elsewhere: a clean merge, without a question.
ok "a line up" l line new up main -q
cd $LAYR_ROOT/projects/p/lines/up
ok "up shows main" l overlay use main -q
eq "up has the version of main" 10 "$(tail -1 lib.c)"
sed_inplace -e 's/^1$/one-up/' lib.c
ok "up changes the first line in main" l save -qam "up one"
cd $M
out=$(l merge up 2>&1)
has "the merge merges the patch with the parent" "lib.c: merged with the parent" "$out"
eq "main has the change of up" one-up "$(l show head:lib.c | head -1)"
eq "and no patch" 10 "$(l show head:lib.c | tail -1)"
eq "pro has both changes" "one-up ten-pro" "$(l overlay show pro head:lib.c | sed -n '1p;$p' | paste -sd' ' -)"
eq "the folder too" "one-up ten-pro" "$(sed -n '1p;$p' lib.c | paste -sd' ' -)"
eq "the folder is clean" "" "$(l status --porcelain)"
eq "nothing waits" "" "$(paths waiting)"
# A local change of the patched path: the patch waits, and no state of pro is recorded.
cd $LAYR_ROOT/projects/p/lines/up
sed_inplace -e 's/^2$/two-up/' lib.c
ok "up changes the second line" l save -qam "up two"
cd $M
echo "/* local */" >> lib.c
pro_before=$(l overlay use | awk '$1=="pro"{print $2}')
has "the merge says that the patch waits" "waits for a merge with the parent" "$(l merge up 2>&1)"
eq "no state of pro is recorded" "$pro_before" "$(l overlay use | awk '$1=="pro"{print $2}')"
has "status says that it waits" "pro: waits for a merge with the parent: lib.c" "$(l status)"
ok "the local change goes" l restore lib.c
has "overlay use merges it" "lib.c: merged with the parent" "$(l overlay use pro 2>&1)"
eq "the folder has the change of up" two-up "$(sed -n 2p lib.c)"
eq "nothing waits after it" "" "$(paths waiting)"

# 3. An overlap: conflict markers, resolved by a save.
cd $LAYR_ROOT/projects/p/lines/up
l merge -q main >/dev/null 2>&1
sed_inplace -e 's/^10$/ten-up/' lib.c
ok "up changes the last line in main" l save -qam "up ten"
cd $M
out=$(l merge up 2>&1)
has "the merge reports a conflict with the parent" "lib.c: conflict with the parent" "$out"
ok "the folder has conflict markers" grep -q '^<<<<<<< overlay' lib.c
has "status lists the conflict" "lib.c" "$(l status --porcelain)"
printf 'one-up\n2\n3\n4\n5\n6\n7\n8\n9\nten-both\n' > lib.c
ok "the resolution is a save" l save -qam "resolve"
eq "pro has the resolution" ten-both "$(l overlay show pro head:lib.c | tail -1)"
eq "main keeps the version of up" ten-up "$(l show head:lib.c | tail -1)"
eq "nothing waits" "" "$(paths waiting)"
eq "the folder is clean" "" "$(l status --porcelain)"

# 4. The parent deletes a patched path: a conflict of kind parent; the overlay keeps its version.
cd $LAYR_ROOT/projects/p/lines/up
l merge -q main >/dev/null 2>&1
l rm -q lib.c
ok "up deletes lib.c in main" l save -qm "drop lib"
cd $M
has "the merge reports it" "the parent deleted or renamed it" "$(l merge up 2>&1)"
no "main lost lib.c" l show head:lib.c
ok "the folder keeps the version of pro" grep -q ten-both lib.c
eq "it waits" lib.c "$(paths waiting)"
echo "own now" > lib.c
ok "a save resolves it" l save -qam "keep lib"
eq "lib.c is an own file of pro now" "lib.c new.c" "$(paths own)"
eq "nothing waits" "" "$(paths waiting)"

# 5. Equal to the parent is not stored: restore --parent, then a save, drops the entry of pro.
echo "x-pro" > x.c
ok "a patch of x.c" l save -qam "patch x"
ok "pro has x.c" l overlay show pro head:x.c
ok "restore --parent" l restore --parent x.c
eq "the folder has the version of main" x "$(cat x.c)"
ok "the save" l save -qam "x back"
no "pro dropped x.c" l overlay show pro head:x.c
eq "the folder is clean" "" "$(l status --porcelain)"
rm y.c
ok "a delete of y.c" l save -qam "drop y"
eq "pro hides y.c" y.c "$(meta | field removed)"
ok "restore --parent of a hidden file" l restore --parent y.c
eq "it is back" y "$(cat y.c)"
ok "the save" l save -qam "y back"
eq "pro hides nothing" "" "$(meta | field removed)"
echo "same as main" > a2.c
cp b.c b.keep
echo "b-pro" > b.c
ok "a patch of b.c" l save -qam "patch b"
cp b.keep b.c; rm b.keep
ok "an edit back to the version of main" l save -qam "b as main"
no "pro dropped b.c" l overlay show pro head:b.c
rm a2.c
cd $LAYR_ROOT/projects/p/lines/up
has "restore --parent in the view of main is refused" "has no parent" "$(l restore --parent a.c 2>&1)"
cd $M

# 6. A merge of the overlay between lines merges the metadata by structure, never as text.
ok "a line w in the view of pro" l line new w main -q
cd $LAYR_ROOT/projects/p/lines/w
eq "w shows pro" pro "$(l overlay use | cut -f1)"
rm a.c
ok "w hides a.c" l save -qam "hide a"
cd $M
rm b.c
ok "main hides b.c" l save -qam "hide b"
ok "main merges w" l merge -q w
eq "pro hides both" "a.c b.c" "$(meta | field removed)"
no "and has no conflict" bash -c "l status --porcelain | grep -q '^UU'"
no "the folder has neither" test -e a.c -o -e b.c
ok "fsck is clean" l fsck

# 7. The automatic states that keep the working content of a view in the overlay's store (docs/overlays.md
# section 1): a limited history keeps the ones that a live automatic state names, and gc drops them when the
# automatic state of the project goes (here with its line).
O=$LAYR_ROOT/projects/p/meta/overlays
kept() { for id in $(sqlite3 "$O/$1/meta/db/layr.sqlite" "select json_extract(value, '\$') from config where key like 'auto.%'"); do [ -e "$O/$1/states/$id" ] && echo "$id"; done | wc -l | tr -d ' '; }
ok "an overlay hist" l overlay create hist -q
l config overlay.hist.history 1 >/dev/null
ok "a line hl" l line new hl main -q
cd $LAYR_ROOT/projects/p/lines/hl
ok "hl shows hist" l overlay use hist -q
echo h1 > h.c
l add h.c
ok "a save to hist" l save -qm h1
echo work >> h.c
ok "an automatic state of the work" l save -q --force
echo h2 > h2.c
l add h2.c
ok "a named save moves hist (history 1)" l save -qm h2
cd $M
n=$(kept hist)
no "the history limit keeps the working content that a live automatic state names" test "$n" = 0
# A deleted line keeps its states while it is in the ring of undoable deletes (undo.deleted); with no ring
# its automatic states go at gc, and the working content in hist with them.
l config undo.deleted 0 >/dev/null
ok "delete the line" l line delete -D --discard hl
ok "gc" l gc -q
eq "gc drops the working content of the deleted line" 0 "$(kept hist)"

finish
