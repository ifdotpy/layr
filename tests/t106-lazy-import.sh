#!/bin/bash
# ci: overlayfs macos-plain
# An imported history keeps its records; a tree is built from git when a command reads it
# (docs/ownership.md, "States imported from Git"). A 200-commit history whose 1 MB file changes
# in every commit costs about one tree on disk, not one per commit.
. "$(dirname "$0")/lib.sh"

U=$T/up; R=$T/remote.git
git init -q --bare -b main $R
git init -q -b main $U; cd $U
git config user.email tester@example.com; git config user.name Tester
seq 1 150000 > big.txt; mkdir small
for i in $(seq 1 200); do
    echo "change $i" >> big.txt
    echo "$i" > small/f$((i % 10)).txt
    git add -A; git commit -qm "c$i"
done
git remote add origin $R; git push -q origin main
C=$(git rev-list --reverse HEAD)
commit_of() { echo "$C" | sed -n "$1p"; } # commit_of <n>: the n-th commit of the history
C10=$(commit_of 10); C20=$(commit_of 20); C30=$(commit_of 30); C31=$(commit_of 31); C130=$(commit_of 130)

l init p --from $U -q >/dev/null
P=$LAYR_ROOT/projects/p
cd $P/lines/main

# Trees on disk: one complete state or thin layer per tree.
trees() { echo $(( $(ls -A $P/states 2>/dev/null | wc -l) + $(ls -A $P/deltas 2>/dev/null | wc -l) )); }

eq "log lists all 200 imported commits" 200 "$(l log --json | wc -l | tr -d ' ')"
eq "only the tip tree is on disk after init" 1 "$(trees)"
ok "the store costs about one tree, not 200 (under 20 MB)" test "$(du -sk $P | cut -f1)" -lt 20000

# A fetch of 50 commits writes records only.
cd $U; for i in $(seq 201 250); do echo "change $i" >> big.txt; git add -A; git commit -qm "c$i"; done; git push -q origin main
cd $P/lines/main
before=$(trees); l git fetch -q origin
eq "a fetch of 50 commits builds no tree" "$before" "$(trees)"
eq "the fetch imported the 50 new commits" 250 "$(l log --json origin/main | wc -l | tr -d ' ')"

# Reads of imported states come from git: a log with diffs, a file and a diff build no tree.
now() { python3 -c 'import time; print(time.time())'; }
start=$(now)
l log -p > $T/logp.out
lazy_seconds=$(python3 -c "print($(now) - $start)")
has "log -p shows the diff of commit 10" "+change 10" "$(cat $T/logp.out)"
eq "log -p over the 200 commits builds no tree" 1 "$(trees)"

git -C $U show $C10:big.txt > $T/git10.out
l show $C10:big.txt > $T/show10.out
ok "layr show of commit 10 gives its file" cmp -s $T/show10.out $T/git10.out
eq "show of commit 10 builds no tree" 1 "$(trees)"

l diff $C10 $C20 > $T/d1020.out
has "layr diff of commits 10 and 20 names the changed file" "big.txt" "$(cat $T/d1020.out)"
eq "diff of commits 10 and 20 builds no tree" 1 "$(trees)"

l line new -q old $C10 >/dev/null
ok "line new at commit 10 has its file" cmp -s $P/lines/old/big.txt $T/git10.out
(cd $P/lines/old && find . -path ./.git -prune -o -type f -print | sed 's|^\./||' | sort) > $T/old.files
git -C $U ls-tree -r --name-only $C10 | sort > $T/git.files
eq "line new at commit 10 has the files of that commit" "$(cat $T/git.files)" "$(cat $T/old.files)"
# The same diff from two built trees gives the same output as the diff read from git.
l line new -q chk20 $C20 >/dev/null
l diff $C10 $C20 > $T/d1020b.out
ok "diff of built states equals the diff read from git" cmp -s $T/d1020.out $T/d1020b.out

CMD='! grep -qx "change 130" big.txt'
RESULT=$(l bisect run --json --good $C10 --bad head -- sh -c "$CMD")
eq "bisect over the imported range finds commit 130" "$(l rev-parse $C130)" "$(l rev-parse "$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')")"

# A state built for a command that needs its tree (try) stays among the newest `import.trees` built trees. The
# next builds push it out and free it in that command, with no gc, and it is built again when a command needs it.
l diff $C30 $C31 > $T/d3031.out
has "layr diff of commits 30 and 31 names the changed file" "big.txt" "$(cat $T/d3031.out)"
l config import.trees 1 >/dev/null
l try -q $C30 -- true
l try -q $C31 -- true
built=$(trees)
# Commits that bisect and diff did not build: each try builds one. C7 first: C3 is no base of it.
l try -q $(commit_of 7) -- true
l try -q $(commit_of 3) -- true
# The states that the ring let go and no owner keeps went in the build that pushed them out: two builds add one
# tree at most (t155 checks the state that goes).
echo "diag: trees $built before the builds, $(trees) after, ring $(ls $P/meta/built 2>/dev/null | wc -l | tr -d ' ')"
ok "the ring bounds the built trees" test "$(trees)" -le $((built + 1))
eq "gc --repair finds no built state to free" "freed 0 built Git state(s) (rebuilt on read)" "$(l gc --repair 2>&1 | grep 'built Git state')"
ok "the tip and the old line keep their trees" test "$(l log --json | wc -l | tr -d ' ')" -eq 200
l diff $C30 $C31 > $T/d3031b.out
ok "a freed state reads again with the same content" cmp -s $T/d3031.out $T/d3031b.out
l try -q $C30 -- true
ok "the freed state is built again on the next command that needs it" test "$(trees)" -gt 2
ok "built states were more than the tip" test "$built" -gt 1

# The same log with every tree on disk (the tree path, as before the change) in the same run. The
# log that reads from git must take at most three times as long.
l config import.trees 1000 >/dev/null
for c in $C; do l try -q $c -- true; done
ok "every commit of the history has its tree on disk (at least 200 entries)" test "$(trees)" -ge 200
start=$(now)
l log -p > $T/logp-built.out
built_seconds=$(python3 -c "print($(now) - $start)")
ok "log -p from git takes at most 3x the tree path ($lazy_seconds s against $built_seconds s)" python3 -c "import sys; sys.exit(0 if $lazy_seconds <= 3 * $built_seconds else 1)"
ok "the log from git equals the log from trees" cmp -s <(sed -E 's/^state [0-9a-f]+$/state X/; s/^Date: .*/Date: X/' $T/logp.out) <(sed -E 's/^state [0-9a-f]+$/state X/; s/^Date: .*/Date: X/' $T/logp-built.out)

finish
