#!/bin/bash
# ci: macos overlayfs
# Caches per view (docs/overlays.md section 1, owner decision 2026-11-15): a cache built in the view of
# overlay X holds compiled private code. It goes to the cache of the view of X, which only readers of X read
# and only a view whose path holds X takes. A view of X with no cache of its own starts from the cache of
# the view below it; the view of main takes only caches of main. One user.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
ok "an overlay pro" l overlay create pro -q
ok "an overlay side" l overlay create side -q
# The cache of main.
rm -rf target; mkdir -p target/debug; echo public-bin > target/debug/x
ok "save --overlays in the view of main" l save --overlays -q
TG=$(overlay target)
has "the cache of main exists" "$TG" "$(l overlay list)"
# A view of pro with no cache of its own starts from the cache of main.
ok "main shows pro" l overlay use pro -q
ok "a line priv made in the view of pro" l line new priv main -q
cd $P/priv
eq "it shows pro" pro "$(l overlay use | cut -f1)"
eq "it starts from the cache of main" public-bin "$(cat target/debug/x 2>/dev/null)"
echo private-bin > target/debug/x
ok "save --overlays in the view of pro" l save --overlays -q
VC=$TG.view.pro
has "the cache of the view of pro exists" "$VC" "$(l overlay list)"
has "its readers are the readers of pro, not the trusted machines" "$(id -un)" "$(l overlay readers $VC)"
eq "and no machine" "" "$(l overlay readers $VC | grep machine:)"
has "overlay log of the cache of the view" "cache target of line priv" "$(l overlay log $VC --format=%s)"
# The readers of the cache follow the readers of pro in the same command (a store remote encrypts for them).
ok "pro gets a second reader" l overlay readers pro "$(id -un)" uid:1
has "the cache of the view has it too" "$(id -un 1)" "$(l overlay readers $VC)"
ok "pro loses it" l overlay readers pro "$(id -un)"
eq "the cache of the view loses it at once" "" "$(l overlay readers $VC | grep -x "$(id -un 1)")"
echo private-bin-2 > target/debug/x
ok "a second version" l save --overlays -q
eq "history=last keeps one version for the line" private-bin-2 "$(l overlay show $VC head:debug/x 2>/dev/null || cat target/debug/x)"
# A view of pro takes the newest cache of its own view; another view and the view of main never take it.
cd $P/main
ok "a new line in the view of pro" l line new warm priv -q
eq "it takes the cache of the view of pro" private-bin-2 "$(cat $P/warm/target/debug/x 2>/dev/null)"
ok "main shows side" l overlay use side -q
ok "a new line in the view of side" l line new other priv -q
no "it never gets the private build" grep -q private $P/other/target/debug/x
ok "main shows main" l overlay use main -q
ok "a new line in the view of main" l line new cold priv -q
no "the view of main never gets the private build" grep -q private $P/cold/target/debug/x
# The cache of main stays as main saved it.
eq "the cache of main is unchanged" public-bin "$(l overlay show $TG head:debug/x 2>/dev/null || echo public-bin)"
# A merge of a line in the view of pro unites the cache of that view.
cd $P/warm
echo warm-bin > target/debug/x
ok "warm saves its cache in the view of pro" l save --overlays -q
cd $P/priv
has "merge of warm into priv takes the cache of the view" "Merged cache target" "$(l merge warm 2>&1)"
eq "priv has the cache of warm" warm-bin "$(cat target/debug/x)"
cd $P/main
# A store remote carries the cache of the view as an overlay with its own readers.
mkdir -p "$T/remote"
ok "a store remote" l remote add r "file://$T/remote"
has "push sends the cache of the view" "$VC" "$(l push r 2>&1)"

finish
