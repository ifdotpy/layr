#!/bin/bash
# Proprietary patches and another project reader (docs/overlays.md, section 9.3): a private edit of a
# shared file in a line that shows an overlay with `patches` is never in a state that another reader of
# the project reads (an automatic state, the shared head) and never reaches the git remote. Needs root
# and real users (the service mode of Linux); t258 has the one-user part.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-p1 layr-p2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 3600 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
L=$T/layr
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=pp LAYR_LINE=$line $L "$@"; }
sh_in() { local u=$1 line=$2; shift 2; as $u sh -c "cd $LAYR_ROOT/projects/pp/lines/$line && $*"; }
# A read of the other reader, who owns no line of the project.
reader() { as layr-p2 $L --project pp --line main "$@"; }
MARK=proprietary-5d2e

mkdir -p $T/src
printf 'int core(void) { return 0; }\n' > $T/src/core.c
l init pp --from $T/src --owner layr-p1 -q >/dev/null
l --project pp access grant layr-p2 reader
cd /tmp || exit 1
ok "the owner makes a work line" in_line layr-p1 main line new -q work main
ok "an overlay for patches that only its owner reads" in_line layr-p1 work overlay create pro --patches -q
in_line layr-p1 work overlay use --add pro >/dev/null 2>&1
has "the line shows it" "pro" "$(in_line layr-p1 work overlay use)"
git init -q --bare -b main $T/r.git; chown -R layr-p1 $T/r.git
l --project pp git remote add origin $T/r.git

# 1. No target yet: a named save refuses, and an automatic state holds no patch.
sh_in layr-p1 work "echo '/* $MARK */' >> core.c"
no "a named save refuses without a target" in_line layr-p1 work save -qam "pro patch"
in_line layr-p1 work save -q --force
ok "the reader reads the automatic state" reader show work@{0}:core.c
eq "it holds no patch" 0 "$(reader show work@{0}:core.c | grep -c "$MARK")"

# 2. The target is the overlay: the save keeps the patch private.
ok "the owner sets the target" in_line layr-p1 work line target pro
ok "the save goes to the overlay" in_line layr-p1 work save -qam "pro patch"
in_line layr-p1 work save -q --force
eq "the automatic state after the save holds no patch" 0 "$(reader show work@{0}:core.c | grep -c "$MARK")"
eq "the shared head of the line holds no patch" 0 "$(reader show work:core.c | grep -c "$MARK")"
no "the reader cannot read the overlay" reader overlay show pro head:core.c
ok "the owner reads the patch there" in_line layr-p1 work overlay show pro head:core.c
ok "push the line" in_line layr-p1 work git push -q origin work
eq "the git remote has no patch" 0 "$(git --git-dir=$T/r.git log --all -p | grep -c "$MARK")"

finish
