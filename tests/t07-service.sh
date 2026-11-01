#!/bin/bash
# The root service: users as clients, permissions of lines, states and the log,
# review of an exact state with `layr try`, automatic states while processes run.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1 layr-u2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 2 > $T/daemon.log 2>&1 &
DPID=$!
trap 'if [ "$FAIL" -ne 0 ]; then cat "$T/daemon.log"; fi; kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET LAYR_SESSION=$u PATH=/usr/bin:/bin "$@"; }
L=$T/layr

sample $T/src
l init perm --from $T/src --owner layr-admin -q >/dev/null
# An open project (everyone a writer) whose main line is not protected: this file tests the
# service and the owners of lines; t13 tests roles and protected lines.
l --project perm access grant '*' writer; l --project perm protect main --remove
P=$LAYR_ROOT/projects/perm/lines
cd $P/main
eq "the admin works through the service" "" "$(as layr-admin $L status -s)"
as layr-admin $L config snapshot.interval 3
as layr-admin $L line new -q w1 --owner layr-u1
eq "the line folder belongs to the user" "layr-u1 700" "$(stat -c '%U %a' $P/w1)"

cd $P/w1
as layr-u1 sh -c "echo a1 > a1.txt && sed -i 's/line 3/line three/' notes.txt && $L add -A && $L save -qm 'a1 work'"
eq "the user commits" "a1 work" "$(as layr-u1 $L log -1 --format=%s)"
no "the user cannot list states" as layr-u1 ls $LAYR_ROOT/projects/perm/states
no "the user cannot read the log" as layr-u1 cat $LAYR_ROOT/projects/perm/meta/log
no "the user cannot remove states" as layr-u1 rm -rf $LAYR_ROOT/projects/perm/states
no "the user cannot enter the main line" as layr-u1 ls $P/main
no "the user cannot merge into the main line" as layr-u1 env LAYR_PROJECT=perm LAYR_LINE=main $L merge w1
cd /
no "another user cannot change the line" as layr-u2 env LAYR_PROJECT=perm LAYR_LINE=w1 $L reset --hard head~1
cd $P/w1
has "the user reads history through layr" "a1.txt" "$(as layr-u1 $L show --stat head)"
no "the user cannot change the project configuration" as layr-u1 $L config x 1
no "the user cannot run gc" as layr-u1 $L gc

# Review of an exact state, then a merge of that state.
S=$(as layr-u1 $L rev-parse head)
cd $P/main
eq "try runs in a copy of the exact state" "0" "$(as layr-admin $L try -q $S -- sh -c 'test -f a1.txt && grep -q three notes.txt'; echo $?)"
has "the check is recorded" "exit 0" "$(as layr-admin $L attestations $S --kind check)"
eq "a failing check is recorded too" "1" "$(as layr-admin $L try -q $S -- false; echo $?)"
as layr-admin $L merge -q w1 --expect $S
eq "merge of the reviewed state" "a1" "$(as layr-admin cat a1.txt)"
cd $P/w1 && as layr-u1 sh -c "echo more >> a1.txt && $L save -qam more" && cd $P/main
eq "--expect refuses a newer state" "1" "$(as layr-admin $L merge w1 --expect $S >/dev/null 2>&1; echo $?)"
as layr-admin $L config push.require_check true
git init -q --bare $T/r.git; chown -R layr-admin $T/r.git; as layr-admin $L git remote add origin $T/r.git
as layr-admin sh -c "echo admin > admin.txt && $L add admin.txt && $L save -qm admin"
has "push needs a passing check" "no passing check" "$(as layr-admin $L git push origin t 2>&1)"
as layr-admin $L try -q head -- true
eq "push after a check" "0" "$(as layr-admin $L git push -q origin t >/dev/null 2>&1; echo $?)"

# Review findings 2026-10-09: undo and other operations respect line owners.
UNDO_MAIN=$(as layr-admin $L op log -n 1 | awk '{print $1}')
cd $P/w1
no "a user cannot undo an operation on the admin's line" as layr-u1 $L undo --force $UNDO_MAIN
eq "the main line did not move" "admin" "$(cd $P/main && as layr-admin $L log -1 --format=%s)"
no "a user cannot push" as layr-u1 $L git push origin t2
no "a user cannot delete a tag" sh -c "as layr-admin env LAYR_PROJECT=perm LAYR_LINE=main $L tag v1 && as layr-u1 $L tag -d v1"

# A merge driver never reads the target of a link in the merged trees.
cd $P/main
as layr-admin sh -c "printf 'x merge=keep\n' > .layrattributes && echo base > x && $L add -A && $L save -qm attrs"
as layr-admin $L config merge.driver.keep 'cat %O > %A'
as layr-admin $L line new -q d1 --owner layr-u1
cd $P/d1 && as layr-u1 sh -c "rm x && ln -s /etc/shadow x && $L add -A && $L save -qm link" && cd $P/main
as layr-admin $L merge -q d1 >/dev/null 2>&1
as layr-admin sh -c "echo ours > x && $L save -qam ours"
cd $P/d1 && as layr-u1 sh -c "rm x && echo theirs > x && $L add -A && $L save -qm theirs" && cd $P/main
as layr-admin $L merge -q d1 >/dev/null 2>&1
no "no secret content reaches the merged file" grep -q 'root:' x

# Files written by layr belong to the line owner.
cd $P/w1
as layr-u1 sh -c "rm a1.txt && $L restore a1.txt"
eq "restored file owner" "layr-u1" "$(stat -c %U a1.txt)"
as layr-u1 sh -c 'cd "$1" && rm -rf ./*' sh "$P/w1"
as layr-u1 $L restore .
eq "rm -rf of the line is restored" "a1" "$(head -1 a1.txt)"

# Automatic states while a process writes (snapshot.interval 3 s, service tick 2 s).
as layr-u1 sh -c 'echo bg > bg.txt'
# A snapshot can appear before its record. Wait for the committed periodic state.
periodic_log=""
for i in $(seq 1 100); do
    periodic_log=$(as layr-u1 $L log --auto --format=%s -n 3)
    printf '%s\n' "$periodic_log" | grep -qx periodic && break
    sleep 0.2
done
has "the service saved the line" "periodic" "$periodic_log"
has "and marked it as taken while processes ran" "taken while processes ran" "$(as layr-u1 $L show -s head@{0})"

# An idle line is compared once per snapshot.interval, not on every tick: a check that found
# no change counts as the line's last automatic state (tick 1 s, interval 3 s).
# The trace counts btrfs comparisons; the OverlayFS backend compares without btrfs send.
if [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
kill $DPID; wait $DPID 2>/dev/null
sleep 3.5
LAYR_TRACE=1 $T/layr daemon --socket $LAYR_SOCKET --tick 1 > $T/daemon2.log 2>&1 &
DPID=$!
expected=$(find "$P" -mindepth 1 -maxdepth 1 -type d | wc -l)
first=0
# One line can take several comparisons in a cycle: the cycle ends when the count stops growing.
prev=-1
for i in $(seq 1 100); do
    first=$(grep -c "send --no-data" "$T/daemon2.log")
    [ "$first" -ge "$expected" ] && [ "$first" = "$prev" ] && break
    prev=$first
    sleep 0.3
done
ok "the daemon completes its first comparison cycle" test "$first" -ge "$expected"
sleep 1.2
eq "idle lines are not compared again before the interval" "$first" "$(grep -c 'send --no-data' $T/daemon2.log)"
grep -E "periodic save|error" "$T/daemon2.log" | head -5
for i in $(seq 1 100); do
    [ "$(grep -c 'send --no-data' "$T/daemon2.log")" -gt "$first" ] && break
    sleep 0.1
done
ok "they are compared again after the interval" test "$(grep -c 'send --no-data' $T/daemon2.log)" -gt "$first"
fi
finish
