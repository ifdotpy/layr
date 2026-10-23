#!/bin/bash
# Export states to folders, with APFS name rules.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init x --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/x/lines
cd $P/main
git init -q --bare $T/r.git; l git remote add origin $T/r.git

# export
l export $T/exp -q
eq "export writes the working content" "$(cat notes.txt)" "$(cat $T/exp/notes.txt)"
echo e >> notes.txt
eq "incremental export writes one path" "exported" "$(l export $T/exp | awk '{print $1}')"
has "incremental export count" "1 path(s)" "$(echo e2 >> notes.txt; l export $T/exp)"
echo secret > local.log
l export -q $T/exp
no "export leaves ignored files out" test -e $T/exp/local.log
echo secret2 > local2.log; echo t >> notes.txt
l export -q $T/exp
no "an incremental export leaves ignored files out too" test -e $T/exp/local2.log
l export -q --all $T/expall
ok "export --all writes ignored files" test -f $T/expall/local.log
rm local.log local2.log
mkdir -p $T/busy; echo keep > $T/busy/x
eq "export refuses a foreign folder" "1" "$(l export $T/busy >/dev/null 2>&1; echo $?)"
echo A > Readme; echo B > README
eq "export reports names that collide on APFS" "1" "$(l export $T/exp2 >/dev/null 2>&1; echo $?)"
has "the colliding names are listed" "README" "$(l export $T/exp3 2>&1)"
no "colliding names are not written" test -e $T/exp3/README
rm Readme README

echo q >> notes.txt

finish
