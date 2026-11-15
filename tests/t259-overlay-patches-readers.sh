#!/bin/bash
# Proprietary patches and another project reader (docs/overlays.md section 1, write rule): a private edit
# of a file of main, a private new file and a private delete in the view of an overlay are never in a
# state that another reader of the project reads (an automatic state, the head in main) and never reach
# the git remote. Needs root and real users (the service mode of Linux); t258 has the one-user part.
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
printf 'int lib(void) { return 0; }\n' > $T/src/lib.c
l init pp --from $T/src --owner layr-p1 -q >/dev/null
l --project pp access grant layr-p2 reader
cd /tmp || exit 1
ok "the owner makes a work line" in_line layr-p1 main line new -q work main
ok "an overlay for patches that only its owner reads" in_line layr-p1 work overlay create pro -q
in_line layr-p1 work overlay use pro >/dev/null 2>&1
has "the line shows it" "pro" "$(in_line layr-p1 work overlay use)"
git init -q --bare -b main $T/r.git; chown -R layr-p1 $T/r.git
l --project pp git remote add origin $T/r.git

# 1. Working changes of the view of pro: an automatic state holds none of them.
sh_in layr-p1 work "echo '/* $MARK */' >> core.c && echo '/* $MARK */' > pro.c"
in_line layr-p1 work save -q --force
ok "the reader reads the automatic state" reader show work@{0}:core.c
eq "it holds no patch" 0 "$(reader show work@{0}:core.c | grep -c "$MARK")"
no "nor the new private file" reader show work@{0}:pro.c

# 2. A named save in the view of pro keeps the patch private (docs/overlays.md section 1, write rule).
ok "the save goes to pro" in_line layr-p1 work save -qam "pro patch"
sh_in layr-p1 work "echo '/* $MARK 2 */' >> core.c"
in_line layr-p1 work save -q --force
eq "the automatic state after the save holds no patch" 0 "$(reader show work@{0}:core.c | grep -c "$MARK")"
eq "the head of the line in main holds no patch" 0 "$(reader show work:core.c | grep -c "$MARK")"
no "nor the private file" reader show work:pro.c
no "the reader cannot read the overlay" reader overlay show pro head:core.c
ok "the owner reads the patch there" in_line layr-p1 work overlay show pro head:core.c
# A private delete: the reader still reads the file in main.
sh_in layr-p1 work "rm lib.c"
ok "a save of the delete" in_line layr-p1 work save -qam "drop lib"
ok "the reader reads lib.c in main" reader show work:lib.c
ok "push the line" in_line layr-p1 work git push -q origin work
eq "the git remote has no patch" 0 "$(git --git-dir=$T/r.git log --all -p | grep -c "$MARK")"
ok "and keeps lib.c" git --git-dir=$T/r.git show work:lib.c

finish
