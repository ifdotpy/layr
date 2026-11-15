#!/bin/bash
# The cache of the view of an overlay with another user (docs/overlays.md section 1, caches per view): a reader
# of the overlay takes and reads its view cache; a reader that loses the read right of the overlay loses the
# cache at once. Needs root and real users (the service mode of Linux).
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-c1 layr-c2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 3600 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
L=$T/layr
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=vc LAYR_LINE=$line $L "$@"; }
sh_in() { local u=$1 line=$2; shift 2; as $u sh -c "cd $LAYR_ROOT/projects/vc/lines/$line && $*"; }
P=$LAYR_ROOT/projects/vc/lines

mkdir -p $T/src
echo 'target regenerable' > $T/src/.layrattributes
echo 'target/' > $T/src/.gitignore
echo x > $T/src/a.txt
l init vc --from $T/src --owner layr-c1 -q >/dev/null
l --project vc access grant layr-c2 writer
cd /tmp || exit 1
ok "an overlay pro read by layr-c2" in_line layr-c1 main overlay create pro --readers layr-c2 -q
ok "main shows pro" in_line layr-c1 main overlay use pro -q
sh_in layr-c1 main "mkdir -p target && echo private-bin > target/out"
ok "the cache of the view of pro" in_line layr-c1 main save --overlays -q
VC=$(in_line layr-c1 main overlay list | awk '/view\.pro/{print $1}' | sed 's/^\*//' | head -1)
ok "layr-c2 reads it" in_line layr-c1 main overlay show $VC --json
ok "layr-c2 makes a line in the view of pro" in_line layr-c2 main line new w2 main
eq "and takes the cache" private-bin "$(sh_in layr-c2 w2 cat target/out)"
ok "layr-c2 loses the read right of pro" in_line layr-c1 main overlay readers pro layr-c1
no "layr-c2 no longer reads the cache" in_line layr-c2 w2 overlay log $VC
ok "a new line of layr-c2" in_line layr-c2 main line new w3 main
no "has no private build" sh_in layr-c2 w3 "grep -q private-bin target/out"

finish
