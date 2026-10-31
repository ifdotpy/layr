#!/bin/bash
# State ids and git commits: `log` shows the git commit of a state, `rev-parse --git` prints it,
# and every revision argument takes a git commit. A mixed reset names the files it kept. A line
# never starts on unsaved work of an automatic state, and a push refuses the unpushed states of
# another line that a line started on.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; echo 'target/' > .gitignore; git add -A; git commit -qm one
echo two >> a.txt; git commit -qam two; git push -q origin main
c1=$(git rev-parse HEAD~1); c2=$(git rev-parse HEAD)
l git clone $R p -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd $M/main || exit 1
l config regenerable '["target"]' >/dev/null

# 1. Two kinds of id.
has "log shows the git commit of a state" "Git:    $c2" "$(l log -n1)"
eq "rev-parse --git prints the git commit" "$c2" "$(l rev-parse --git head)"
eq "rev-parse --git --short" "${c2:0:7}" "$(l rev-parse --git --short head)"
ok "rev-parse prints the layr id, not the git commit" test "$(l rev-parse head)" != "$c2"
echo local > b.txt; l add b.txt; l save -qm local
eq "rev-parse --git of a new state is the commit of the .git folder" "$(git rev-parse HEAD)" "$(l rev-parse --git head)"
eq "log shows no git commit for an unpushed state" "" "$(l log -n1 | grep '^Git:')"

# 2. Every revision argument takes a git commit.
eq "log takes a git commit" "one" "$(l log -n1 --format=%s $c1)"
eq "show takes a git commit" "one" "$(l show -s --format=%s ${c1:0:10})"
eq "diff takes a git commit" "a.txt b.txt" "$(l diff --name-only $c1 head | tr '\n' ' ' | sed 's/ $//')"
l restore --source $c1 a.txt
eq "restore --source takes a git commit" "one" "$(cat a.txt)"
l restore a.txt

# 3. A mixed reset keeps the files and names them.
l line new -q mix >/dev/null
cd $M/mix || exit 1
echo three >> a.txt; l save -qam three
out=$(l reset head~1 2>$T/err)
has "a mixed reset names the changed files" "M	a.txt" "$out"
has "a mixed reset names the command that changes them" "layr reset --hard head~1" "$(cat $T/err)"
eq "a mixed reset keeps the files" "three" "$(tail -1 a.txt)"
eq "reset -q prints nothing" "" "$(l reset -q head 2>&1)"

# 4. A line starts on an automatic state only when it holds no unsaved work.
cd $M/main || exit 1
mkdir -p target; echo built > target/out
warm=$(l save | awk '{print $NF}')
ok "a line starts on an automatic state with ignored build output only" l line new -q warm $warm
echo unsaved > c.txt
dirty=$(l save | awk '{print $NF}')
out=$(l line new dirty $dirty 2>&1); rc=$?
eq "a line on unsaved work is refused" 1 "$rc"
has "the refusal names the unsaved file" "c.txt" "$out"
no "the refused line does not exist" test -d $M/dirty
rm c.txt

# 5. A push refuses the unpushed states of another line under the line's start.
l line new -q task main >/dev/null
cd $M/task || exit 1
echo task > t.txt; l add t.txt; l save -qm task
out=$(l git push origin task 2>&1); rc=$?
eq "a push of a line started on unpushed work of main is refused" 1 "$rc"
has "the refusal names the state of main" "(main) local" "$out"
no "nothing reached the remote" git -C $R rev-parse -q --verify refs/heads/task
ok "--include-base pushes it" l git push -q --include-base origin task
eq "the push carries the work of main" "b.txt" "$(git -C $R diff --name-only main task -- b.txt)"
# A line from the remote branch carries only its own work; a merge of another line is allowed.
l line new -q clean origin/main >/dev/null
cd $M/clean || exit 1
echo c > c.txt; l add c.txt; l save -qm clean
ok "a push of a line from the remote branch" l git push -q origin clean
cd $M/warm || exit 1
echo w > w.txt; l add w.txt; l save -qm warm
ok "the warm line pushes: its base went out with --include-base" l git push -q origin warm
finish
