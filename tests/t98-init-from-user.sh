#!/bin/bash
# A normal user runs `init --from` through the root daemon. The daemon reads the folder with the
# user's rights and writes the store as root: the store folders are closed to the user.
. "$(dirname "$0")/lib.sh"

if [ "$(uname -s)" != Linux ]; then
    echo "t98-init-from-user.sh (Linux daemon only): 0 passed, 0 failed"
    exit 0
fi

chmod 755 $T $LAYR_ROOT
id layr-u1 >/dev/null 2>&1 || useradd -m layr-u1
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
L=$T/layr

# The user's folder: closed to others, a private file, a hard link pair and a link.
S=$T/u1-src
mkdir -p $S/sub
echo hello > $S/a.txt
ln $S/a.txt $S/a-twin.txt
ln -s a.txt $S/to-a
echo private > $S/sub/b.txt
chmod 600 $S/sub/b.txt
chown -R layr-u1 $S
chmod 700 $S

ok "a user imports a folder through the daemon" as layr-u1 $L init p1 --from $S -q
W=$LAYR_ROOT/projects/p1/lines/main
eq "the file content is imported" hello "$(cat $W/a.txt 2>&1)"
eq "a private file of the user is imported" private "$(cat $W/sub/b.txt 2>&1)"
eq "a link is imported as a link" a.txt "$(readlink $W/to-a 2>&1)"
eq "hard links stay one file" "$(stat_field inode $W/a.txt)" "$(stat_field inode $W/a-twin.txt)"
eq "the private file keeps its mode" 600 "$(stat_field mode $W/sub/b.txt)"
if [ -d $LAYR_ROOT/projects/p1/states ]; then
    eq "no store file belongs to the user" 0 "$(find $LAYR_ROOT/projects/p1/states -user layr-u1 | wc -l)"
fi

# A file that only root can read stops the import: the daemon reads with the user's rights.
S2=$T/u1-src2
mkdir -p $S2
echo open > $S2/open.txt
chown -R layr-u1 $S2
echo root-only-secret > $S2/root.txt
chmod 600 $S2/root.txt
no "a file the user cannot read stops the import" as layr-u1 $L init p2 --from $S2 -q
eq "the root-only content is not in the store" "" "$(grep -rl root-only-secret $LAYR_ROOT 2>/dev/null)"

finish
