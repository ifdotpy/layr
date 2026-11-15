#!/bin/bash
# ci: macos overlayfs
# Checks per view, smallest step (docs/overlays.md section 1, owner decision D10): the required checks of a
# protected line run on the view of main, so a merge that brings a change of a patches overlay into such a line
# is refused; a merge that changes only main is checked and goes on. One user.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src
echo base > $T/src/a.txt
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
ok "an overlay pro" l overlay create pro -q
l config check.unit 'true' >/dev/null
ok "main needs the check unit" l protect main --check unit
ok "a line pub in the view of main" l line new pub main -q
cd $P/pub
echo pub >> a.txt
ok "a change of main" l save -qam "pub"
cd $P/main
ok "a merge that changes only main runs the check and goes on" l merge -q pub --expect "$(l rev-parse pub)"
eq "main has it" pub "$(l show head:a.txt | tail -1)"
ok "a line priv" l line new priv main -q
cd $P/priv
ok "priv shows pro" l overlay use pro -q
echo secret > s.txt
l add s.txt
ok "a change of pro" l save -qm "priv"
cd $P/main
before=$(l rev-parse head)
out=$(l merge -q priv --expect "$(l rev-parse priv)" 2>&1); rc=$?
eq "a merge that brings a change of pro is refused" 1 "$rc"
has "and names the overlay" "changes overlay 'pro'" "$out"
eq "main did not move" "$before" "$(l rev-parse head)"
no "pro of main has no private file" l overlay show pro main:s.txt

finish
