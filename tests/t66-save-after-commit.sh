#!/bin/bash
# An unchanged line makes no new automatic state, also when the head is newer than the last
# automatic state (a named state was saved after it). The save prints the existing state in the
# format of a new one: the line name and the state id.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; git add -A; git commit -qm one; git push -q origin main
l git clone $R p -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main || exit 1
l line new -q w >/dev/null
cd $LAYR_ROOT/projects/p/lines/w || exit 1

id() { printf %s "$1" | awk '{print $NF}'; }
echo auto > a.txt
auto=$(id "$(l save)")
eq "the first save makes an automatic state" 1 "$(printf %s "$auto" | grep -c '^[0-9a-f]\{32\}$')"
eq "unchanged: the save prints that state" "w $auto" "$(l save)"
echo more > b.txt
l add -A >/dev/null; l save -qm two >/dev/null
head=$(l log -n1 | awk 'NR==1{print $2}')
eq "status is clean" "nothing to save, working tree clean" "$(l status | tail -1)"
out=$(l save)
if [ "$out" != "w $head" ]; then
    echo "DIAG save printed: $out; head: $head"
    echo "DIAG status:"; l status
    echo "DIAG diff head..new:"; l diff --name-status "$head" "$(id "$out")"
    echo "DIAG log:"; l log -n3 | head -20
fi
eq "after a named state, the save prints the head" "w $head" "$out"
eq "a second save prints the same" "$out" "$(l save)"
echo three >> b.txt
new=$(id "$(l save)")
eq "a changed file makes a new state" 1 "$(printf %s "$new" | grep -c '^[0-9a-f]\{32\}$')"
eq "unchanged again: the save prints that state" "w $new" "$(l save)"
finish
