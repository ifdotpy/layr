#!/bin/bash
# Who may do what (docs/design.md, section 3): roles, deny rules, owners of lines, protected
# lines with checks on the merge result; configured commands run as the caller; push keeps the
# history.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
getent group layr-bots >/dev/null || groupadd layr-bots
for u in layr-admin layr-u1 layr-u2 layr-u3 layr-u4; do id $u >/dev/null 2>&1 || useradd -m $u; done
usermod -aG layr-bots layr-u3
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
# A command of a user in a line (the line folder belongs to its owner only).
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=p LAYR_LINE=$line $T/layr "$@"; }
L=$T/layr
sample $T/src
l init p --from $T/src --owner layr-admin -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd /tmp || exit 1

# 1. A new project: only its admin, and main is protected.
eq "the creator is the only principal" "layr-admin	admin" "$(l --project p access)"
has "another user cannot read the project" "may not read project p" "$(as layr-u1 $L --project p --line main log 2>&1)"
eq "main is protected" "main	update maintainer	expect true	check -	direct false	rewrite admin" "$(l --project p protect)"

# 2. Roles and deny rules.
as layr-admin $L --project p access grant layr-u1 writer
as layr-admin $L --project p access grant layr-u2 maintainer
as layr-admin $L --project p access grant @layr-bots writer
as layr-admin $L --project p access deny @layr-bots push sync
no "a writer cannot change roles" as layr-u1 $L --project p access grant layr-u1 admin
ok "a writer reads the project" as layr-u1 $L --project p --line main log --oneline
ok "a writer makes its own line" as layr-u1 $L --project p branch -q u1 main
no "a writer cannot make a line for another user" as layr-u1 $L --project p branch -q x1 main --owner layr-u2
ok "a maintainer can" as layr-u2 $L --project p branch -q for-u1 main --owner layr-u1
has "access check explains a deny rule" "deny rule for @layr-bots" "$(l --project p access check layr-u3 push)"
# A role for a time adds to the user's role and ends by itself.
as layr-admin $L --project p access grant layr-u1 maintainer --until 3s
has "it is listed with its end" "maintainer	until" "$(l --project p access | grep layr-u1)"
ok "while it lasts it works" as layr-u1 $L --project p branch -q tmp1 main --owner layr-u2
sleep 4
no "after it ends it does not" as layr-u1 $L --project p branch -q tmp2 main --owner layr-u2
eq "the user keeps the role it had" "layr-u1	writer" "$(l --project p access | grep layr-u1)"
eq "the owner of a line reads the project without a role" "0" "$(l --project p branch -q own-only main --owner layr-admin; echo $?)"
# A line owner without a role (a line made for it by a maintainer) may export and try.
l --project p branch -q u4-line main --owner layr-u4
eq "it has no role" "refused" "$(l --project p access check layr-u4 export | cut -d: -f1)"
ok "a line owner without a role exports" as layr-u4 env LAYR_PROJECT=p LAYR_LINE=u4-line $T/layr export HEAD /tmp/layr-u4-export-$$ -q
ok "and tries" as layr-u4 env LAYR_PROJECT=p LAYR_LINE=u4-line $T/layr try -q HEAD -- true

# 3. Only the owner changes a line, the admin included.
cd $P/u1 && as layr-u1 sh -c "echo u1 > u1.txt && $L add u1.txt && $L commit -qm u1" && cd /tmp
no "the admin cannot commit in another user's line" in_line layr-admin u1 commit --allow-empty -qm intrude
no "the admin cannot reset another user's line" in_line layr-admin u1 reset -q --hard HEAD~1
ok "a maintainer can delete another user's line" as layr-u2 $L --project p branch -D for-u1
eq "the line is gone" "" "$(l --project p branch | grep for-u1)"

# 4. A protected line: no direct changes, merges only of a reviewed state, checks on the result.
no "the owner of main cannot commit in it" as layr-admin sh -c "cd $P/main && echo x >> notes.txt && $L commit -qam direct"
as layr-admin sh -c "cd $P/main && $L restore notes.txt"
U1=$(in_line layr-u1 u1 rev-parse HEAD)
no "a writer cannot merge into main" in_line layr-u1 main merge -q u1 --expect $U1
has "a merge without --expect is refused" "add --expect" "$(in_line layr-u2 main merge -q u1 2>&1)"
l --project p config check.unit 'test ! -e broken.txt'
l --project p protect main --check unit
cd $P/u1 && as layr-u1 sh -c "echo bad > broken.txt && $L add broken.txt && $L commit -qm broken" && cd /tmp
U1=$(in_line layr-u1 u1 rev-parse HEAD)
before=$(l --project p --line main rev-parse HEAD)
out=$(in_line layr-u2 main merge -q u1 --expect $U1 2>&1); rc=$?
eq "a failing check refuses the merge" "1" "$rc"
has "and names the check" "check 'unit' failed" "$out"
eq "main did not move" "$before" "$(l --project p --line main rev-parse HEAD)"
cd $P/u1 && as layr-u1 sh -c "$L rm -q broken.txt && $L commit -qm fixed" && cd /tmp
U1=$(in_line layr-u1 u1 rev-parse HEAD)
ok "a passing check lets the merge through" in_line layr-u2 main merge -q u1 --expect $U1
eq "main has the change" "u1" "$(l --project p show main:u1.txt)"
has "the check is recorded on the merged state" "test ! -e broken.txt" "$(l --project p --line main attestations HEAD --kind check)"
# A check runs code of the merged state: it does not see the caller's environment.
l --project p config check.env 'test -z "$SECRET_OF_CALLER"'
l --project p protect main --check unit,env
cd $P/u1 && as layr-u1 sh -c "echo e > e.txt && $L add e.txt && $L commit -qm e" && cd /tmp
ok "a check does not get the caller's secrets" as layr-u2 env SECRET_OF_CALLER=s3cr3t LAYR_PROJECT=p LAYR_LINE=main $T/layr merge -q u1 --expect $(in_line layr-u1 u1 rev-parse HEAD)
l --project p protect main --check unit
# A merge that makes a new state: the check runs on exactly that state.
ok "a bot makes a line" as layr-u3 $L --project p branch -q bot main
cd $P/bot && as layr-u3 sh -c "echo b > bot.txt && $L add bot.txt && $L commit -qm bot" && cd /tmp
cd $P/u1 && as layr-u1 sh -c "echo u2 > u2.txt && $L add u2.txt && $L commit -qm u2" && cd /tmp
ok "first merge" in_line layr-u2 main merge -q bot --expect $(in_line layr-u3 bot rev-parse HEAD)
ok "a second merge makes a merge state and checks it" in_line layr-u2 main merge -q u1 --expect $(in_line layr-u1 u1 rev-parse HEAD)
eq "it is a merge" "2" "$(l --project p --line main log -1 --format=%p | wc -w)"
has "checked" "test ! -e broken.txt" "$(l --project p --line main attestations HEAD --kind check)"

# Path rules: a change of CI configuration needs the admin, even through a maintainer's merge.
l --project p protect main --path .github=admin
has "the rule lists the path" "paths .github=admin" "$(l --project p protect)"
cd $P/u1 && as layr-u1 sh -c "mkdir -p .github && echo ci > .github/ci.yml && $L add .github && $L commit -qm ci" && cd /tmp
out=$(in_line layr-u2 main merge -q u1 --expect $(in_line layr-u1 u1 rev-parse HEAD) 2>&1); rc=$?
eq "a maintainer cannot merge a change of .github" "1" "$rc"
has "and is told which path needs whom" "'.github/ci.yml', which in line 'main' needs the role admin" "$out"
ok "the admin can" as layr-admin env LAYR_PROJECT=p LAYR_LINE=main $T/layr merge -q u1 --expect $(in_line layr-u1 u1 rev-parse HEAD)
l --project p protect main --path .github=

# Approvals: the reviewed state needs an approval of exactly that state by a member of the
# approvers group who is not one of its authors.
getent group layr-humans >/dev/null || groupadd layr-humans
usermod -aG layr-humans layr-admin
l --project p protect main --approvals 1 --approvers @layr-humans
has "the rule lists the approvals" "approvals 1 from @layr-humans" "$(l --project p protect)"
cd $P/u1 && as layr-u1 sh -c "echo r > r.txt && $L add r.txt && $L commit -qm r" && cd /tmp
R=$(in_line layr-u1 u1 rev-parse HEAD)
has "without an approval the merge is refused" "after 1 approval(s)" "$(in_line layr-u2 main merge -q u1 --expect $R 2>&1)"
in_line layr-u1 u1 approve -q $R
in_line layr-u2 main approve -q $R
has "approvals of the author and of a non-member do not count" "it has 0" "$(in_line layr-u2 main merge -q u1 --expect $R 2>&1)"
as layr-admin env LAYR_PROJECT=p LAYR_LINE=main $T/layr approve -q $R
eq "approvals are listed" "3" "$(l --project p --line main attestations $R --kind approval --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
as layr-admin env LAYR_PROJECT=p LAYR_LINE=main $T/layr approve -q --withdraw $R
has "a withdrawn approval does not count" "it has 0" "$(in_line layr-u2 main merge -q u1 --expect $R 2>&1)"
as layr-admin env LAYR_PROJECT=p LAYR_LINE=main $T/layr approve -q $R
cd $P/u1 && as layr-u1 sh -c "echo r2 >> r.txt && $L commit -qam r2" && cd /tmp
R2=$(in_line layr-u1 u1 rev-parse HEAD)
has "an approval binds to the exact state" "it has 0" "$(in_line layr-u2 main merge -q u1 --expect $R2 2>&1)"
ok "the approved state merges" in_line layr-u2 main merge -q $R --expect $R
l --project p protect main --approvals 0

# 5. Moving a protected line back needs its rewrite role (admin).
no "a maintainer cannot undo a merge into main" in_line layr-u2 main undo --force
MOP=$(l --project p op log -n 30 | grep -w merge | head -1 | awk '{print $1}')
want=$(l --project p --line main rev-parse 'HEAD~1')
ok "the admin can" as layr-admin $L --project p undo --force $MOP
eq "main went back" "$want" "$(l --project p --line main rev-parse HEAD)"
ok "and forward again" in_line layr-u2 main merge -q u1 --expect $(in_line layr-u1 u1 rev-parse HEAD)

# 6. A deny rule wins over any role.
as layr-admin $L --project p access grant layr-u3 maintainer
has "a maintainer in a denied group cannot push" "deny rule for @layr-bots" "$(in_line layr-u3 main push origin main 2>&1)"
as layr-admin $L --project p access grant layr-u3 admin
as layr-admin $L --project p access deny @layr-bots access
no "an admin denied 'access' cannot change a check command" as layr-u3 $L --project p config check.unit true
ok "but changes other settings" as layr-u3 $L --project p config retention.caches 5
as layr-admin $L --project p access allow @layr-bots access
as layr-admin $L --project p access grant layr-u3 maintainer

# 7. Configured merge commands run as the caller, never as another user.
cd $P/main && printf 'w.txt merge=who\n' > .layrattributes && echo 0 > w.txt && l add -A && l commit -qm attrs && cd /tmp
l --project p config merge.driver.who 'id -un > %A'
as layr-u1 $L --project p branch -q u1b main
cd $P/main && echo main > w.txt && l commit -qam main-w && cd /tmp
cd $P/u1b && as layr-u1 sh -c "echo u1 > w.txt && $L commit -qam u1-w && $L merge -q main >/dev/null 2>&1; cat w.txt" > $T/who.txt; cd /tmp
eq "the driver ran as the caller of the merge" "layr-u1" "$(cat $T/who.txt)"

# 8. push keeps the history of the line; --squash makes one commit.
git init -q --bare $T/r.git
l --project p remote add origin $T/r.git
l --project p --line main push -q origin main
eq "every state of main is a commit" "$(l --project p --line main log --format=%s | wc -l)" "$(git --git-dir=$T/r.git rev-list --count main)"
ok "merges stay merges" test "$(git --git-dir=$T/r.git rev-list --merges --count main)" -ge 1
eq "the subject of each commit is the state's message" "$(l --project p --line main log --format=%s | head -3)" "$(git --git-dir=$T/r.git log --format=%s -3 main)"
cd $P/u1 && as layr-u1 sh -c "echo a > a.txt && $L add a.txt && $L commit -qm a && echo b > b.txt && $L add b.txt && $L commit -qm b" && cd /tmp
l --project p --line u1 push -q --squash -m "task" origin task
eq "--squash pushes one commit" "1" "$(git --git-dir=$T/r.git rev-list --count main..task)"
eq "with the given message" "task" "$(git --git-dir=$T/r.git log -1 --format=%s task)"
# The git objects are history too: readable by the roles, a group's members included.
eq "the git store is closed" "750" "$(stat -c %a $LAYR_ROOT/projects/p/git)"
ok "a member of a group with a role reads git objects" runuser -u layr-u3 -- ls $LAYR_ROOT/projects/p/git/objects
no "a user without a role does not" runuser -u nobody -- ls $LAYR_ROOT/projects/p/git/objects

# 9. A reader without a line reads every state through the service and changes nothing. The
# socket is open to every user; the service decides by the caller's uid and role.
eq "the socket is open to every user" "666" "$(stat -c %a $LAYR_SOCKET)"
id layr-r1 >/dev/null 2>&1 || useradd -m layr-r1
has "a user without a role is refused by the service, not by the socket" "may not read project p" "$(as layr-r1 $L --project p show main:notes.txt 2>&1)"
as layr-admin $L --project p access grant layr-r1 reader
eq "a reader reads the log of a line" "$(l --project p --line main log --oneline)" "$(as layr-r1 $L --project p --line main log --oneline)"
eq "a reader shows a file of a state" "$(l --project p show main:notes.txt)" "$(as layr-r1 $L --project p show main:notes.txt)"
eq "a reader diffs two states" "$(l --project p diff main~1 main)" "$(as layr-r1 $L --project p diff main~1 main)"
eq "a reader diffs states of two lines" "$(l --project p diff main u1)" "$(as layr-r1 $L --project p diff main u1)"
eq "a reader lists the files of a line" "$(l --project p --line main ls-files)" "$(as layr-r1 $L --project p --line main ls-files)"
eq "a reader searches a state" "$(l --project p grep -n 'line 1' main)" "$(as layr-r1 $L --project p grep -n 'line 1' main)"
eq "a reader resolves a state" "$(l --project p rev-parse u1)" "$(as layr-r1 $L --project p rev-parse u1)"
ok "a reader tests another user's state in a temporary copy" as layr-r1 $L --project p try -q u1 -- test -f a.txt
eq "the try copy is gone" "" "$(ls -a $P | grep '^\.try-')"
has "HEAD needs a line" "add --line <name>" "$(as layr-r1 $L --project p log 2>&1)"
has "a command outside a line names --project" "--project <name>" "$(as layr-r1 $L log 2>&1)"
no "a reader makes no line" as layr-r1 $L --project p branch -q r1 main
no "a reader does not commit in another user's line" in_line layr-r1 u1 commit --allow-empty -qm intrude
finish
