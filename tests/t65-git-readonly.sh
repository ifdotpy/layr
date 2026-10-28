#!/bin/bash
# The .git folder of a line is read only for git: git writes fail and change nothing, git reads work,
# and layr (save, merge, push, reset, line delete) still works.
. "$(dirname "$0")/lib.sh"

# Root ignores the read only mode, so as root the git commands run as a user that owns the test
# line and has a reader grant for the store. Elsewhere they run as the caller.
if [ "$(id -u)" = 0 ]; then
    GIT_USER=layr-git-u
    id $GIT_USER >/dev/null 2>&1 || useradd -m $GIT_USER
    chmod 755 $T $LAYR_ROOT
    git_as() { runuser -u $GIT_USER -- env PATH="$PATH" git -c safe.directory='*' "$@"; }
else
    GIT_USER=
    git_as() { git "$@"; }
fi

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; git add -A; git commit -qm "upstream 1"; git push -q origin main
l git clone $R ro -q >/dev/null
cd $LAYR_ROOT/projects/ro/lines/main || exit 1
[ -z "$GIT_USER" ] || l access grant $GIT_USER reader >/dev/null
l line new -q side >/dev/null
echo s > $LAYR_ROOT/projects/ro/lines/side/side.txt
sidec=$(cd $LAYR_ROOT/projects/ro/lines/side && l add side.txt && l save -qm side >/dev/null && git rev-parse HEAD)
echo two > b.txt; l add b.txt; l save -qm two >/dev/null
head=$(git rev-parse HEAD); sum=$(cat a.txt b.txt | cksum)
eq "git rev-parse works" 40 "$(printf %s "$head" | wc -c | tr -d ' ')"

check() { # check <description> <git args...>: the write fails and nothing changes
    local d=$1; shift
    out=$(git_as "$@" 2>&1); rc=$?
    eq "$d fails" refused "$([ "$rc" -ne 0 ] && echo refused)"
    has "$d: permission denied" "ermission denied" "$out"
    eq "$d leaves HEAD" "$head" "$(git rev-parse HEAD)"
    eq "$d leaves the files" "$sum" "$(cat a.txt b.txt | cksum)"
}
# The worktree parent is writable for the git user, so only the sealed .git can refuse it.
wt_parent=$T/wt-parent; mkdir -p $wt_parent
[ -z "$GIT_USER" ] || chown $GIT_USER $wt_parent $LAYR_ROOT/projects/ro/lines/main
echo three > c.txt; git_as add c.txt 2>/dev/null
check "git commit" commit --allow-empty -qm nope
check "git merge" merge --no-edit -q $sidec
check "git checkout" checkout -q "$head"
check "git reset" reset -q --hard HEAD~1
echo dirty >> b.txt; sum=$(cat a.txt b.txt | cksum)
check "git stash" stash
check "git worktree add" worktree add -q "$wt_parent/wt" HEAD
ok "git status works" git_as status --short
ok "git log works" git_as log --oneline
ok "git diff works" git_as diff
ok "git show works" git_as show --stat HEAD
ok "git rev-parse works" git_as rev-parse HEAD
no "no worktree was made" test -e "$wt_parent/wt"
rm -f c.txt; l restore b.txt >/dev/null 2>&1 || echo two > b.txt

# layr itself still works in the sealed line.
echo more >> b.txt
ok "save works" l save -a -qm more
ok "git HEAD follows the save" test "$(git rev-parse HEAD)" != "$head"
ok "merge works" l merge side
ok "push works" l git push -q origin main
eq "the remote has the line head" "$(git rev-parse HEAD)" "$(git -C $R rev-parse main)"
eq "line is still sealed after the save" 555 "$(stat_field mode .git)"

# After a git move of HEAD, the new refusal text works: write HEAD in place, then save.
ex=$(git rev-parse HEAD)
chmod -R u+w .git; git commit -q --allow-empty -m outside
out=$(l save -a -qm blocked 2>&1); rc=$?
eq "save refuses after a git commit" refused "$([ "$rc" -ne 0 ] && echo refused)"
has "the refusal tells what to run" "printf '%s\\n' $ex > .git/HEAD" "$out"
printf '%s\n' $ex > .git/HEAD
echo again >> b.txt
ok "save works after the HEAD fix" l save -a -qm fixed
eq ".git is sealed again" 555 "$(stat_field mode .git)"

# An older line gets sealed at its next refresh, and line delete works on a sealed line.
chmod -R u+w .git
echo x >> b.txt; l save -a -qm x >/dev/null
eq "refresh seals a writable .git" 555 "$(stat_field mode .git)"
cd $LAYR_ROOT/projects/ro/lines/side || exit 1
eq "side line is sealed" 555 "$(stat_field mode .git)"
cd $LAYR_ROOT/projects/ro/lines/main || exit 1
ok "line delete of a sealed line" l line delete -D side
no "the deleted line is gone" test -e $LAYR_ROOT/projects/ro/lines/side
finish
