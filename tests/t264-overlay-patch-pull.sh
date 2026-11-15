#!/bin/bash
# ci: macos overlayfs
# A patch merges with its parent when `layr git pull` and a follow pass move main under the view of the
# overlay (docs/overlays.md section 1, "a patch is a patch"). One user.
. "$(dirname "$0")/lib.sh"
mkdir -p $T/home
if [ "$(uname -s)" = Darwin ]; then export HOME=$T/home; else export LAYR_SOCKET=$T/service.sock; fi

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
seq 1 10 > lib.c
git add -A; git commit -qm base; git push -q origin main
l git clone $R gm -q >/dev/null
P=$LAYR_ROOT/projects/gm/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
ok "an overlay pro" l overlay create pro -q
ok "main shows pro" l overlay use pro -q
sed_inplace -e 's/^10$/ten-pro/' lib.c
ok "a patch of lib.c" l save -qam "patch"

# 1. git pull brings a change of the first line: the patch merges with it.
cd $U; sed_inplace -e 's/^1$/one-up/' lib.c; git commit -qam up1; git push -q origin main
cd $P/main
out=$(l git pull origin main 2>&1)
has "git pull merges the patch with the parent" "lib.c: merged with the parent" "$out"
eq "main has the change of the remote" one-up "$(l show head:lib.c | head -1)"
eq "pro has both" "one-up ten-pro" "$(l overlay show pro head:lib.c | sed -n '1p;$p' | paste -sd' ' -)"
eq "the folder too" "one-up ten-pro" "$(sed -n '1p;$p' lib.c | paste -sd' ' -)"
eq "the folder is clean" "" "$(l status --porcelain)"

# 2. A follow pass brings a change of the second line: the patch merges again.
l follow origin/main >/dev/null 2>&1
cd $U; git pull -q origin main 2>/dev/null; sed_inplace -e 's/^2$/two-up/' lib.c; git commit -qam up2; git push -q origin main
cd $P/main
ok "a follow pass" l follow --run
eq "main has the change of the pass" two-up "$(l show head:lib.c | sed -n 2p)"
eq "pro merged it" "two-up ten-pro" "$(l overlay show pro head:lib.c | sed -n '2p;$p' | paste -sd' ' -)"
eq "the folder too" "two-up ten-pro" "$(sed -n '2p;$p' lib.c | paste -sd' ' -)"
eq "the folder is clean after the pass" "" "$(l status --porcelain)"
l follow --remove >/dev/null 2>&1

finish
