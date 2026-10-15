#!/bin/bash
# The git object store under a strict umask (0077, as a service unit may set): owners of lines
# read its objects through the alternates of their .git folder, a user without a role does not
# get past the store folder, and a store of an earlier layr gets read access once.
. "$(dirname "$0")/lib.sh"

umask 077
chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1 layr-u2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
start_daemon() { # start_daemon <tick>
    $T/layr daemon --socket $LAYR_SOCKET --tick $1 > $T/daemon.log 2>&1 &
    DPID=$!
    for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
}
start_daemon 60
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
L=$T/layr
S=$T/src
mkdir -p $S && echo a > $S/a.txt
git -C $S init -q -b main && git -C $S add a.txt && git -C $S commit -qm first
l init p --from $S --owner layr-admin -q >/dev/null
l --project p access grant layr-u1 writer
P=$LAYR_ROOT/projects/p/lines
G=$LAYR_ROOT/projects/p/git
cd /tmp || exit 1

as layr-u1 $L --project p branch -q a1 main
ok "a line owner reads the history with git" as layr-u1 git -C $P/a1 log --oneline
echo later > $T/blob
oid=$(git --git-dir=$G hash-object -w $T/blob)
ok "and an object that the service writes later" as layr-u1 git -C $P/a1 cat-file -e $oid
no "a user without a role does not get into the store" as layr-u2 cat $G/HEAD

# A store of an earlier layr: only its folder was open to the readers.
earlier() { chmod -R go-rwx $G/* && git --git-dir=$G config --unset core.sharedRepository; }
earlier
no "the store of an earlier layr is closed to the line owner" as layr-u1 git -C $P/a1 log --oneline
ok "a new line" as layr-u1 $L --project p branch -q a2 main
ok "repairs it" as layr-u1 git -C $P/a1 log --oneline
no "a user without a role still does not get in" as layr-u2 cat $G/HEAD
earlier
kill $DPID && wait $DPID 2>/dev/null
start_daemon 1
for i in $(seq 1 50); do as layr-u1 git -C $P/a1 log --oneline >/dev/null 2>&1 && break; sleep 0.2; done
ok "the service repairs it soon after a start" as layr-u1 git -C $P/a1 log --oneline
no "and keeps a user without a role out" as layr-u2 cat $G/HEAD
finish
