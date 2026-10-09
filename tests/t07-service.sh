#!/bin/bash
# The root service: agents as their own Linux users, permissions of lines, states and the log,
# review of an exact state with `layr try`, automatic states while processes run.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-lead layr-a1 layr-a2; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 2 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET LAYR_AGENT=$u PATH=/usr/bin:/bin "$@"; }
L=$T/layr

sample $T/src
l init perm --from $T/src --owner layr-lead -q >/dev/null
P=$LAYR_ROOT/projects/perm/lines
cd $P/main
eq "the lead works through the service" "" "$(as layr-lead $L status -s)"
as layr-lead $L config snapshot.interval 3
as layr-lead $L branch -q w1 --owner layr-a1
eq "the line folder belongs to the agent" "layr-a1 700" "$(stat -c '%U %a' $P/w1)"

cd $P/w1
as layr-a1 sh -c "echo a1 > a1.txt && sed -i 's/line 3/line three/' notes.txt && $L add -A && $L commit -qm 'a1 work'"
eq "the agent commits" "a1 work" "$(as layr-a1 $L log -1 --format=%s)"
no "the agent cannot list states" as layr-a1 ls $LAYR_ROOT/projects/perm/states
no "the agent cannot read the log" as layr-a1 cat $LAYR_ROOT/projects/perm/meta/log
no "the agent cannot remove states" as layr-a1 rm -rf $LAYR_ROOT/projects/perm/states
no "the agent cannot enter the main line" as layr-a1 ls $P/main
no "the agent cannot merge into the main line" as layr-a1 env LAYR_PROJECT=perm LAYR_LINE=main $L merge w1
cd /
no "another agent cannot change the line" as layr-a2 env LAYR_PROJECT=perm LAYR_LINE=w1 $L reset --hard HEAD~1
cd $P/w1
has "the agent reads history through layr" "a1.txt" "$(as layr-a1 $L show --stat HEAD)"
no "the agent cannot change the project configuration" as layr-a1 $L config x 1
no "the agent cannot run gc" as layr-a1 $L gc

# Review of an exact state, then a merge of that state.
S=$(as layr-a1 $L rev-parse HEAD)
cd $P/main
eq "try runs in a copy of the exact state" "0" "$(as layr-lead $L try -q $S -- sh -c 'test -f a1.txt && grep -q three notes.txt'; echo $?)"
has "the check is recorded" "exit 0" "$(as layr-lead $L checks $S)"
eq "a failing check is recorded too" "1" "$(as layr-lead $L try -q $S -- false; echo $?)"
as layr-lead $L merge -q w1 --expect $S
eq "merge of the reviewed state" "a1" "$(as layr-lead cat a1.txt)"
cd $P/w1 && as layr-a1 sh -c "echo more >> a1.txt && $L commit -qam more" && cd $P/main
eq "--expect refuses a newer state" "1" "$(as layr-lead $L merge w1 --expect $S >/dev/null 2>&1; echo $?)"
as layr-lead $L config push.require_check true
git init -q --bare $T/r.git; chown -R layr-lead $T/r.git; as layr-lead $L remote add origin $T/r.git
as layr-lead sh -c "echo lead > lead.txt && $L add lead.txt && $L commit -qm lead"
has "push needs a passing check" "no passing check" "$(as layr-lead $L push origin t 2>&1)"
as layr-lead $L try -q HEAD -- true
eq "push after a check" "0" "$(as layr-lead $L push -q origin t >/dev/null 2>&1; echo $?)"

# Review findings 2026-10-09: undo and other operations respect line owners.
UNDO_MAIN=$(as layr-lead $L op log -n 1 | awk '{print $1}')
cd $P/w1
no "an agent cannot undo an operation on the lead's line" as layr-a1 $L undo --force $UNDO_MAIN
eq "the main line did not move" "lead" "$(cd $P/main && as layr-lead $L log -1 --format=%s)"
no "an agent cannot push" as layr-a1 $L push origin t2
no "an agent cannot delete a tag" sh -c "as layr-lead env LAYR_PROJECT=perm LAYR_LINE=main $L tag v1 && as layr-a1 $L tag -d v1"
no "an agent cannot put another agent's line in a group" sh -c "as layr-lead env LAYR_PROJECT=perm LAYR_LINE=main $L group create g1 && as layr-a2 env LAYR_PROJECT=perm $L group add g1 w1"

# A merge driver never reads the target of a link in the merged trees.
cd $P/main
as layr-lead sh -c "printf 'x merge=keep\n' > .layrattributes && echo base > x && $L add -A && $L commit -qm attrs"
as layr-lead $L config merge.driver.keep 'cat %O > %A'
as layr-lead $L branch -q d1 --owner layr-a1
cd $P/d1 && as layr-a1 sh -c "rm x && ln -s /etc/shadow x && $L add -A && $L commit -qm link" && cd $P/main
as layr-lead $L merge -q d1 >/dev/null 2>&1
as layr-lead sh -c "echo ours > x && $L commit -qam ours"
cd $P/d1 && as layr-a1 sh -c "rm x && echo theirs > x && $L add -A && $L commit -qm theirs" && cd $P/main
as layr-lead $L merge -q d1 >/dev/null 2>&1
no "no secret content reaches the merged file" grep -q 'root:' x

# Files written by layr belong to the line owner.
cd $P/w1
as layr-a1 sh -c "rm a1.txt && $L restore a1.txt"
eq "restored file owner" "layr-a1" "$(stat -c %U a1.txt)"
as layr-a1 sh -c 'rm -rf ./*'
as layr-a1 $L restore .
eq "rm -rf of the line is restored" "a1" "$(head -1 a1.txt)"

# Automatic states while a process writes (snapshot.interval 3 s, service tick 2 s).
as layr-a1 sh -c 'echo bg > bg.txt'
sleep 7
has "the service saved the line" "periodic" "$(as layr-a1 $L log --auto --format=%s -n 3)"
has "and marked it as taken while processes ran" "taken while processes ran" "$(as layr-a1 $L show -s HEAD@{0})"
finish
