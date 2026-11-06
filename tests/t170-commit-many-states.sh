#!/bin/bash
# ci: macos
# A full Git commit id that maps to several states (a state that a push exported and the import of the same commit
# on another machine) resolves to one of them: they hold the same tree. A short prefix of two commits stays
# ambiguous. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; git add -A; git commit -qm c1; git push -q origin main
C1=$(git rev-parse HEAD)
l git clone $R it -q >/dev/null
cd $LAYR_ROOT/projects/it/lines/main || exit 1
S1=$(l rev-parse "$C1")
ok "the commit resolves to its state" test -n "$S1"
echo two > b.txt; l add b.txt; l save -qm two
S2=$(l rev-parse head)
# The same commit also maps to another state, as an export and an import of one commit on two machines do.
DB=$LAYR_ROOT/projects/it/meta/db/layr.sqlite
python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute("insert into git_map(state,gcommit) values(?,?)", (sys.argv[2], sys.argv[3])); c.commit()' "$DB" "$S2" "$C1"
out=$(l rev-parse "$C1" 2>&1); rc=$?
eq "a commit with two states resolves" 0 "$rc"
ok "to one of its states" sh -c "[ '$out' = '$S1' ] || [ '$out' = '$S2' ]"
ok "a line starts from it" l line new -q from-commit "$C1"
finish
