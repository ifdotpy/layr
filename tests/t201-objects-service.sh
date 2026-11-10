#!/bin/bash
# ci: overlayfs
# layr objects checkout through the root service (docs/objects.md, section 13): every file of the
# checkout belongs to the calling user (root writes nothing there), content, links, modes and times
# are right, and a folder that the caller cannot write is refused. Linux only: the root service.
. "$(dirname "$0")/lib.sh"

if [ "$(uname -s)" != Linux ] || [ "$(id -u)" != 0 ]; then
    echo "$(basename "$0") (Linux root service only): 0 passed, 0 failed"
    exit 0
fi

chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 2 > $T/daemon.log 2>&1 &
DPID=$!
trap 'if [ "$FAIL" -ne 0 ]; then cat "$T/daemon.log"; fi; kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET LAYR_SESSION=$u PATH=/usr/bin:/bin "$@"; }
L=$T/layr

sample $T/src
mkdir -p $T/src/bin $T/src/empty
printf '#!/bin/sh\necho hi\n' > $T/src/bin/run.sh
chmod 755 $T/src/bin/run.sh
ln -s ../notes.txt $T/src/bin/link
l init objs --from $T/src --owner layr-admin -q >/dev/null
l --project objs access grant '*' writer
l --project objs protect main --remove
P=$LAYR_ROOT/projects/objs/lines
cd $P/main
S=$(l rev-parse head)
ok "the admin imports the state through the service" as layr-admin $L objects import head

cd /
mkdir -m 755 $T/u1
chown layr-u1 $T/u1
ok "a user checks out the state through the service" as layr-u1 $L --project objs objects checkout $S $T/u1/out
eq "every file of the checkout belongs to the user" "" "$(find $T/u1/out ! -user layr-u1 | head -3)"
eq "the file content" "$(cat $P/main/notes.txt)" "$(cat $T/u1/out/notes.txt)"
eq "the symbolic link" ../notes.txt "$(readlink $T/u1/out/bin/link)"
ok "the executable mode" test -x $T/u1/out/bin/run.sh
ok "the empty folder" test -d $T/u1/out/empty
eq "the modification time" "$(stat -c %Y $P/main/notes.txt)" "$(stat -c %Y $T/u1/out/notes.txt)"
ok "the user can change the checkout" as layr-u1 sh -c "echo mine >> $T/u1/out/notes.txt"

mkdir -m 755 $T/rootdir
no "a folder that the user cannot write is refused" as layr-u1 $L --project objs objects checkout $S $T/rootdir/out
no "and nothing appears there" test -e $T/rootdir/out

finish
