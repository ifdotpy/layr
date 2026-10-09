#!/bin/bash
# Regressions from the security review (2026-10-09): commands as root through merge drivers,
# links inside lines, backup files, child process environment, peer trust, record order,
# limits of the service.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-lead layr-a1 layr-a2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
LAYR_MAX_REQUESTS_PER_USER=3 SECRET_OF_SERVICE=hidden $T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
L=$T/layr
sample $T/src
l init s --from $T/src --owner layr-lead -q >/dev/null
P=$LAYR_ROOT/projects/s/lines
cd $P/main

# 1. Merge drivers never run as root for a non-root caller.
no "the lead cannot make a root-owned line" as layr-lead $L branch -q r0 --owner 0
no "the lead cannot change the lead" as layr-lead $L config lead 0
as layr-lead sh -c "printf 'u.txt merge=who\n' > .layrattributes && echo 0 > u.txt && $L add -A && $L commit -qm attrs"
as layr-lead $L config merge.driver.who 'id -u > %A'
as layr-lead $L branch -q d1 --owner layr-a1
cd $P/d1 && as layr-a1 sh -c "echo 1 > u.txt && $L commit -qam d1" && cd $P/main
as layr-lead sh -c "echo 2 > u.txt && $L commit -qam main"
as layr-lead $L merge -q d1 >/dev/null 2>&1
eq "the driver ran as the lead" "$(id -u layr-lead)" "$(cat u.txt)"

# 2. A link inside a line never redirects a write of the service.
cd $P/d1
mkdir -p $T/outside && chmod 777 $T/outside
as layr-a1 sh -c "mkdir -p docs && echo v1 > docs/a.txt && $L add -A && $L commit -qm docs"
as layr-a1 sh -c "rm -rf docs && ln -s $T/outside docs"
as layr-a1 $L restore docs/a.txt >/dev/null 2>&1
no "restore did not write through the link" test -e $T/outside/a.txt
eq "folders made by layr belong to the line owner" "layr-a1" "$(stat -c %U docs)"
as layr-a1 sh -c "rm -rf docs && $L restore docs/a.txt"
eq "restore works when the folder is real again" "v1" "$(cat docs/a.txt)"
as layr-a1 sh -c "ln -s /etc linked-etc"
no "grep does not read through a link in the line" sh -c "as layr-a1 $L grep -l root 2>/dev/null | grep -q shadow"

# 3. Backups are written as the writer.
mkdir -p $T/bk && chown layr-lead $T/bk
cd $P/main && as layr-lead $L backup -q $T/bk
eq "backup files belong to the lead" "layr-lead" "$(stat -c %U $(ls -d $T/bk/*/chain-0001/* | head -1))"
no "a backup into a folder the caller cannot write is refused" as layr-lead $L backup -q /root

# 4. Child processes get the user's environment and groups, not the service's.
cd $P/main
no "try does not see the service environment" as layr-lead $L try -q HEAD -- sh -c 'test -n "$SECRET_OF_SERVICE"'
eq "try runs with the user's groups" "$(id -G layr-lead)" "$(as layr-lead $L try -q HEAD -- id -G)"

# 5. Peer records: trust and local settings.
new_fs $T/b 3G
BR=$T/b/store
LAYR_ROOT=$BR l init other --from $T/src -q >/dev/null
LAYR_ROOT=$BR l --project other op export > $T/other.jsonl
has "records of another project are refused" "another project" "$(l --project s op import $T/other.jsonl 2>&1)"
cd $P/main
l sync -q -- env LAYR_ROOT=$BR $LAYR serve-peer s
LAYR_ROOT=$BR l --project s config merge.driver.who 'touch /tmp/pwned-by-peer; true'
LAYR_ROOT=$BR l --project s config remote.origin.url /tmp/evil.git
cd $P/main && l sync -q -- env LAYR_ROOT=$BR $LAYR serve-peer s
eq "a peer does not change local merge commands" "id -u > %A" "$(l config merge.driver.who)"
eq "a peer does not set local remotes" "" "$(l config remote.origin.url)"
# A third machine that nobody trusts.
new_fs $T/c 3G
CR=$T/c/store
LAYR_ROOT=$BR l --project s sync -q -- env LAYR_ROOT=$CR $LAYR serve-peer s
LAYR_ROOT=$CR l --project s config retention.auto 7
# Only C's own records: nobody that A trusts vouched for C in them.
CID=$(cat $CR/machine/id)
LAYR_ROOT=$CR l --project s op export | python3 -c "
import json, sys
for l in sys.stdin:
    if json.loads(l)['r']['machine'] == '$CID': sys.stdout.write(l)" > $T/c.jsonl
has "records of an untrusted machine are refused" "untrusted" "$(l --project s op import $T/c.jsonl 2>&1)"
key=$(LAYR_ROOT=$CR l --project s machines | sed -n 's/^\* .* key \([0-9a-f]*\)$/\1/p')
l --project s machines trust $CID --key $key >/dev/null
has "an explicit trust takes them" "imported" "$(l --project s op import $T/c.jsonl 2>&1)"

# 7. The result does not depend on the order of records.
before=$(l --project s config --list | md5sum)
l --project s reindex >/dev/null
eq "settings after a rebuild equal the incremental result" "$before" "$(l --project s config --list | md5sum)"

# 8. Limits of the service.
runuser -u layr-lead -- python3 - "$LAYR_SOCKET" <<'PY' &
import socket, sys, time
s = [socket.socket(socket.AF_UNIX) for _ in range(3)]
for x in s: x.connect(sys.argv[1])
time.sleep(4)
PY
sleep 1
has "a user over the limit gets a busy answer" "busy" "$(as layr-lead $L status 2>&1)"
sleep 12
eq "silent clients time out and free their places" "0" "$(cd $P/main && as layr-lead $L status -s >/dev/null 2>&1; echo $?)"
finish
