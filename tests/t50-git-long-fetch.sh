#!/bin/bash
# A remote 300 commits ahead: the fetch imports every commit within the bound, and it reports
# its progress while it holds the store lock.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; for i in $(seq 1 20); do echo "base $i" > src/f$i.txt; done
git add -A; git commit -qm "upstream 0"; git push -q origin main

l git clone $R gp -q >/dev/null
cd $U
for i in $(seq 1 300); do echo "change $i" >> src/f$((i % 20 + 1)).txt; git commit -qam "upstream $i"; done
git push -q origin main

cd $LAYR_ROOT/projects/gp/lines/main || exit 1
timeout 900 "$LAYR" git fetch origin main > $T/fetch.out 2> $T/fetch.err
eq "the fetch of 300 commits ends with success" 0 "$?"
eq "the newest commit is imported" "upstream 300" "$(l log -1 --format=%s origin/main)"
eq "every commit is imported" "301" "$(l log --format=%s origin/main | wc -l | tr -d ' ')"
has "the import reports its progress at 50 commits" "imported 50 commits" "$(cat $T/fetch.err)"
has "the import reports its progress at 300 commits" "imported 300 commits" "$(cat $T/fetch.err)"
finish
