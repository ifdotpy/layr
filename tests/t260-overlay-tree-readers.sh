#!/bin/bash
# The readers rule of the tree of overlays with other users (docs/overlays.md section 1, owner decision
# 2026-11-15): a reader of an overlay reads every overlay below it; a new line shows the view of the folder
# where `line new` ran, cut below the first overlay that its owner does not read; another view of a line
# needs the read right of every overlay on its path. Needs root and real users (the service mode of Linux).
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-t1 layr-t2 layr-t3; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 3600 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
L=$T/layr
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=tr LAYR_LINE=$line $L "$@"; }
P=$LAYR_ROOT/projects/tr/lines

mkdir -p $T/src
printf 'open\n' > $T/src/open.txt
l init tr --from $T/src --owner layr-t1 -q >/dev/null
l --project tr access grant layr-t2 writer
l --project tr access grant layr-t3 writer
cd /tmp || exit 1

# 1. The tree: team (read by layr-t2) on main, pro on team.
ok "an overlay team for layr-t2" in_line layr-t1 main overlay create team --readers layr-t2 -q
ok "pro above team, for layr-t2 too" in_line layr-t1 main overlay create pro --parent team --readers layr-t2 -q
has "a reader of pro who does not read team is refused" "but not its ancestor 'team'" "$(in_line layr-t1 main overlay create pro2 --parent team --readers layr-t3 2>&1)"
has "team cannot drop a reader of pro" "which is above 'team'" "$(in_line layr-t1 main overlay readers team layr-t1 2>&1)"
ok "main shows pro" in_line layr-t1 main overlay use pro -q
as layr-t1 sh -c "cd $P/main && echo team > team.txt && echo pro > pro.txt"
in_line layr-t1 main overlay move -q team team.txt
in_line layr-t1 main overlay move -q pro pro.txt
ok "a save of both overlays" in_line layr-t1 main save -qm "team and pro"

# 2. A new line takes the view of the folder where `line new` ran.
ok "a reader of both makes a line in the folder of main" in_line layr-t2 main line new -q w2 main
eq "it shows team and pro" "team pro" "$(in_line layr-t2 w2 overlay use | cut -f1 | paste -sd' ')"
ok "with the file of pro" test -e $P/w2/pro.txt
ok "a non-reader makes a line in the folder of main" in_line layr-t3 main line new -q w3 main
no "its folder has no file of team" test -e $P/w3/team.txt
no "nor of pro" test -e $P/w3/pro.txt
no "it cannot use pro" in_line layr-t3 w3 overlay use pro
no "nor make a view of pro" in_line layr-t3 w3 view main --overlay pro
# 3. A reader of team only: the new line stops below pro.
ok "layr-t3 reads team" in_line layr-t1 main overlay readers team layr-t2 layr-t3
ok "layr-t3 makes another line in the folder of main" in_line layr-t3 main line new -q w4 main
eq "it shows team only" "team" "$(in_line layr-t3 w4 overlay use | cut -f1 | paste -sd' ')"
no "and not the file of pro" test -e $P/w4/pro.txt

finish
