#!/bin/bash
# Regressions from the security review (2026-10-09): commands as root through merge drivers,
# links inside lines, child process environment, peer trust, record order,
# limits of the service.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1 layr-u2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
LAYR_MAX_REQUESTS_PER_USER=3 SECRET_OF_SERVICE=hidden $T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
L=$T/layr
sample $T/src
l init s --from $T/src --owner layr-admin -q >/dev/null
# An open project (everyone a writer) whose main line is not protected: this file tests the
# service and the owners of lines; t13 tests roles and protected lines.
l --project s access grant '*' writer; l --project s protect main --remove
P=$LAYR_ROOT/projects/s/lines
cd $P/main

# 1. Merge drivers never run as root for a non-root caller.
no "the admin cannot make a root-owned line" as layr-admin $L branch -q r0 --owner 0
no "the admin cannot change the admin" as layr-admin $L config admin 0
as layr-admin sh -c "printf 'u.txt merge=who\n' > .layrattributes && echo 0 > u.txt && $L add -A && $L commit -qm attrs"
as layr-admin $L config merge.driver.who 'id -u > %A'
as layr-admin $L branch -q d1 --owner layr-u1
cd $P/d1 && as layr-u1 sh -c "echo 1 > u.txt && $L commit -qam d1" && cd $P/main
as layr-admin sh -c "echo 2 > u.txt && $L commit -qam main"
as layr-admin $L merge -q d1 >/dev/null 2>&1
eq "the driver ran as the admin" "$(id -u layr-admin)" "$(cat u.txt)"
# A driver that leaves a link instead of its result does not make the service read the target.
as layr-admin sh -c "printf 'u.txt merge=who\nk.txt merge=leak\n' > .layrattributes && echo 0 > k.txt && $L add -A && $L commit -qm leak"
as layr-admin $L config merge.driver.leak 'rm -f %A && ln -s /etc/shadow %A'
as layr-admin $L branch -q d2 --owner layr-u1
cd $P/d2 && as layr-u1 sh -c "echo 1 > k.txt && $L commit -qam d2" && cd $P/main
as layr-admin sh -c "echo 2 > k.txt && $L commit -qam main2"
eq "a driver result that is a link is a conflict" "1" "$(as layr-admin $L merge -q d2 >/dev/null 2>&1; echo $?)"
no "the merge result has no root file" grep -q root: k.txt
eq "the result keeps ours" "2" "$(cat k.txt)"

# 2. A link inside a line never redirects a write of the service.
cd $P/d1
mkdir -p $T/outside && chmod 777 $T/outside
as layr-u1 sh -c "mkdir -p docs && echo v1 > docs/a.txt && $L add -A && $L commit -qm docs"
as layr-u1 sh -c "rm -rf docs && ln -s $T/outside docs"
as layr-u1 $L restore docs/a.txt >/dev/null 2>&1
no "restore did not write through the link" test -e $T/outside/a.txt
eq "folders made by layr belong to the line owner" "layr-u1" "$(stat -c %U docs)"
as layr-u1 sh -c "rm -rf docs && $L restore docs/a.txt"
eq "restore works when the folder is real again" "v1" "$(cat docs/a.txt)"
as layr-u1 sh -c "ln -s /etc linked-etc"
no "grep does not read through a link in the line" sh -c "as layr-u1 $L grep -l root 2>/dev/null | grep -q shadow"

# 4. Child processes get the user's environment and groups, not the service's.
cd $P/main
no "try does not see the service environment" as layr-admin $L try -q HEAD -- sh -c 'test -n "$SECRET_OF_SERVICE"'
eq "try runs with the user's groups" "$(id -G layr-admin)" "$(as layr-admin $L try -q HEAD -- id -G)"

# 5. Peer records: trust and local settings.
new_fs $T/b 3G
BR=$T/b/store
LAYR_ROOT=$BR l init other --from $T/src -q >/dev/null
LAYR_ROOT=$BR l --project other op export > $T/other.jsonl
has "records of another project are refused" "another project" "$(l --project s op import $T/other.jsonl 2>&1)"
cd $P/main
peer_exchange sync s -q -- env LAYR_ROOT=$BR $LAYR serve-peer s
LAYR_ROOT=$BR l --project s config merge.driver.who 'touch /tmp/pwned-by-peer; true'
LAYR_ROOT=$BR l --project s config remote.origin.url /tmp/evil.git
cd $P/main && peer_exchange sync s -q -- env LAYR_ROOT=$BR $LAYR serve-peer s
eq "a peer does not change local merge commands" "id -u > %A" "$(l config merge.driver.who)"
eq "a peer does not set local remotes" "" "$(l config remote.origin.url)"
# A third machine that nobody trusts.
new_fs $T/c 3G
CR=$T/c/store
LAYR_ROOT=$BR peer_exchange sync s -q -- env LAYR_ROOT=$CR $LAYR serve-peer s
LAYR_ROOT=$CR l --project s config retention.auto 7
# Only C's own records: nobody that A trusts vouched for C in them.
CID=$(cat $CR/machine/id)
LAYR_ROOT=$CR l --project s op export | python3 -c "
import json, sys
for l in sys.stdin:
    if json.loads(l)['r']['machine'] == '$CID': sys.stdout.write(l)" > $T/c.jsonl
has "records of an untrusted machine are refused" "untrusted" "$(l --project s op import $T/c.jsonl 2>&1)"
key=$(LAYR_ROOT=$CR l --project s remote list | sed -n 's/^\* .* key \([0-9a-f]*\)$/\1/p')
l --project s remote trust test-peer $CID --public-key $key >/dev/null
has "an explicit trust takes them" "imported" "$(l --project s op import $T/c.jsonl 2>&1)"

# 7. The result does not depend on the order of records.
before=$(l --project s config --list | md5sum)
l --project s reindex >/dev/null
eq "settings after a rebuild equal the incremental result" "$before" "$(l --project s config --list | md5sum)"

# Second review (2026-10-09).
# Settings of a peer never act here.
LAYR_ROOT=$BR l --project s config retention.auto 1
LAYR_ROOT=$BR l --project s config push.require_check true
cd $P/main && peer_exchange sync s -q -- env LAYR_ROOT=$BR $LAYR serve-peer s
eq "a peer does not change retention here" "" "$(l --project s config retention.auto)"
eq "a peer does not change the push policy here" "" "$(l --project s config push.require_check)"
# Frames from the network have a size limit.
has "a huge frame is refused without allocating it" "frame" "$(printf 'J\377\377\377\377' | timeout 10 $LAYR serve-peer s 2>&1)"
# The schema version: an old database is upgraded from its records, a newer one is refused.
before=$(l --project s --line main log --format=%H | md5sum)
python3 -c "import sqlite3; c=sqlite3.connect('$LAYR_ROOT/projects/s/meta/db/layr.sqlite'); c.execute('PRAGMA user_version=1'); c.execute('DROP TABLE tags'); c.commit()"
eq "an old schema is rebuilt from the records" "$before" "$(l --project s --line main log --format=%H | md5sum)"
python3 -c "import sqlite3; c=sqlite3.connect('$LAYR_ROOT/projects/s/meta/db/layr.sqlite'); c.execute('PRAGMA user_version=99'); c.commit()"
has "a newer schema is refused" "newer layr" "$(l --project s --line main log 2>&1)"
python3 -c "import sqlite3; c=sqlite3.connect('$LAYR_ROOT/projects/s/meta/db/layr.sqlite'); c.execute('PRAGMA user_version=6'); c.commit()"
# Git objects follow the roles.
git init -q --bare $T/gr.git; cd $P/main; l --project s git remote add origin $T/gr.git >/dev/null 2>&1; l --project s --line main git push -q origin t >/dev/null 2>&1
l --project s access revoke '*'
eq "the git store is closed to others" "750" "$(stat -c %a $LAYR_ROOT/projects/s/git)"
no "another user cannot read git objects" runuser -u layr-u2 -- ls $LAYR_ROOT/projects/s/git/objects
ok "the admin can" runuser -u layr-admin -- ls $LAYR_ROOT/projects/s/git/objects
l --project s --line main branch -q acl1 --owner layr-u2
ok "a line owner can read git objects" runuser -u layr-u2 -- ls $LAYR_ROOT/projects/s/git/objects
l --project s --line main branch -D acl1 >/dev/null
no "after the line is deleted it cannot" runuser -u layr-u2 -- ls $LAYR_ROOT/projects/s/git/objects
l --project s access grant '*' reader
eq "with a role for everyone it is open again" "755" "$(stat -c %a $LAYR_ROOT/projects/s/git)"

# 8. Limits of the service.
runuser -u layr-admin -- python3 - "$LAYR_SOCKET" <<'PY' &
import socket, sys, time
s = [socket.socket(socket.AF_UNIX) for _ in range(3)]
for x in s: x.connect(sys.argv[1])
time.sleep(4)
PY
sleep 1
has "a user over the limit gets a busy answer" "busy" "$(as layr-admin $L status 2>&1)"
sleep 12
eq "silent clients time out and free their places" "0" "$(cd $P/main && as layr-admin $L status -s >/dev/null 2>&1; echo $?)"
finish
