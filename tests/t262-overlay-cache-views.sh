#!/bin/bash
# ci: macos overlayfs
# Caches per view (docs/overlays.md section 1, owner decision 2026-11-15): a cache built in the view of an
# overlay holds compiled private code, so it is never saved to a cache overlay, which every view and the
# trusted machines read. A view of main saves its caches as before, and a view of an overlay starts from
# the cache of main. One user.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
ok "an overlay pro" l overlay create pro -q
ok "a line priv" l line new priv main -q
cd $P/priv
ok "it shows pro" l overlay use pro -q
mkdir -p target/debug; echo private-bin > target/debug/x
out=$(l save --overlays 2>&1)
has "save --overlays in the view of pro saves no cache" "are not saved" "$out"
eq "no cache overlay exists" "" "$(l overlay list | grep -c "folder=target" | grep -v '^0$')"
echo "int p;" > p.c
l add p.c
ok "a named save in the view of pro" l save -qm "private"
eq "it saves no cache either" "" "$(l overlay list | grep -c "folder=target" | grep -v '^0$')"
# The view of main saves its cache, and a new line in the view of pro starts from it.
ok "a line pub" l line new pub main -q
cd $P/pub
ok "it shows main" l overlay use main -q
rm -rf target; mkdir -p target/debug; echo public-bin > target/debug/x
ok "save --overlays in the view of main" l save --overlays -q
has "the cache overlay exists" "folder=target" "$(l overlay list)"
ok "pub shows pro" l overlay use pro -q
ok "a new line made in the view of pro" l line new warm pub -q
cd $P/warm
eq "it shows pro" pro "$(l overlay use | cut -f1)"
eq "it starts from the cache of main" public-bin "$(cat target/debug/x 2>/dev/null)"
echo private2 > target/debug/x
ok "save --overlays there" l save --overlays -q
cd $P/pub
ok "pub shows main again" l overlay use main -q
ok "a new line in the view of main" l line new cold pub -q
eq "it never gets the private build" public-bin "$(cat $P/cold/target/debug/x 2>/dev/null)"

finish
