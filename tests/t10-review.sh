#!/bin/bash
# Regressions: links in the .git folder and a failed database write.
. "$(dirname "$0")/lib.sh"

R=$T/r.git; git init -q --bare -b main $R; git clone -q $R $T/up 2>/dev/null
cd $T/up && echo a > a.txt && git add a.txt && git commit -qm a && git push -q origin main
l git clone $R p -q >/dev/null
W=$LAYR_ROOT/projects/p/lines/main
cd $W

# 1. The .git folder belongs to the line owner: links in it must not redirect root writes.
echo victim-head > $T/victim1; echo victim-index > $T/victim2
chmod -R u+w .git # the folder is read only for git; the owner can open it
rm .git/HEAD .git/index; ln -s $T/victim1 .git/HEAD; ln -s $T/victim2 .git/index
echo b > b.txt; l add b.txt; l save -qm b; l git push -q origin t1
eq "a link at .git/HEAD does not redirect the write" "victim-head" "$(cat $T/victim1)"
eq "a link at .git/index does not redirect the write" "victim-index" "$(cat $T/victim2)"
ok "HEAD is a regular file again" test -f .git/HEAD -a ! -L .git/HEAD
ok "git works in the line" git log --oneline -1

# 4. A failed database write records nothing and leaves no snapshot.
states_before=$(ls $LAYR_ROOT/projects/p/states | wc -l)
echo d > d.txt; l add d.txt
chattr +i $LAYR_ROOT/projects/p/meta/db/layr.sqlite $LAYR_ROOT/projects/p/meta/db/layr.sqlite-wal 2>/dev/null
out=$(l save -qm d 2>&1); rc=$?
chattr -i $LAYR_ROOT/projects/p/meta/db/layr.sqlite $LAYR_ROOT/projects/p/meta/db/layr.sqlite-wal 2>/dev/null
eq "the commit fails" "128" "$rc"
eq "no record" "b" "$(l log -1 --format=%s)"
eq "no snapshot is left" "$states_before" "$(ls $LAYR_ROOT/projects/p/states | wc -l)"
l save -qm d
eq "the next commit works" "d" "$(l log -1 --format=%s)"
eq "with its snapshot" "d" "$(l show head:d.txt)"
has "records verify" "signatures ok" "$(l fsck)"
eq "fsck is clean" "0 problem(s)" "$(l fsck | tail -1)"
finish
