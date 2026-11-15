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
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines/main
cd $M
git init -q --bare -b main $T/r.git
l git remote add origin $T/r.git >/dev/null
ok "push the open-source start" l git push -q origin main
leaks() { git --git-dir=$T/r.git log --all -p | grep -c "$MARK"; }
ok "an overlay for proprietary patches" l overlay create pro -q
ok "it may hold versions of shared files" l overlay set pro --patches
l overlay use --add pro >/dev/null 2>&1
has "the line shows it" "pro" "$(l overlay use)"

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

finish
