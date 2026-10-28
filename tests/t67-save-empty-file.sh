#!/bin/bash
# An empty file has no data extents, so it has no data identity. An unchanged empty file (for
# example a cargo lock file in an ignored target folder) must not make every save a new state.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; echo 'target/' > .gitignore; git add -A; git commit -qm one; git push -q origin main
l git clone $R p -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main || exit 1
l line new -q w >/dev/null
cd $LAYR_ROOT/projects/p/lines/w || exit 1

id() { printf %s "$1" | awk '{print $NF}'; }
mkdir -p target/debug; : > target/debug/.cargo-lock; : > empty.txt; echo data > target/debug/f
first=$(id "$(l save)")
eq "the first save makes a state" 1 "$(printf %s "$first" | grep -c '^[0-9a-f]\{32\}$')"
eq "unchanged empty files: the save prints that state" "w $first" "$(l save)"
eq "and again" "w $first" "$(l save)"
echo x >> empty.txt
second=$(id "$(l save)")
eq "a file that got content makes a new state" 1 "$(printf %s "$second" | grep -c '^[0-9a-f]\{32\}$')"
eq "it is another state" 1 "$([ "$first" != "$second" ] && echo 1)"
finish
