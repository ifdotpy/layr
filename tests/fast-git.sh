#!/bin/bash
# Git in a fast line (LayrFS): the line has a `.git` folder, git commands move the line, and layr
# commands move git's branch (docs/commands.md, "Git in a line"). Provide LAYR (on Linux built
# with layrfs-fuse), an isolated LAYR_ROOT, and on Linux LAYR_SOCKET of its root daemon. Run as a
# non-root user. macOS needs the LayrFS extension.
set -u
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
[ "$(uname -s)" = Darwin ] || : "${LAYR_SOCKET:?}"
export LAYR_AUTHOR_NAME=Layr LAYR_AUTHOR_EMAIL=layr@example.invalid GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Dev GIT_AUTHOR_EMAIL=dev@example.invalid GIT_COMMITTER_NAME=Dev GIT_COMMITTER_EMAIL=dev@example.invalid
L=$LAYR
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-git.XXXXXX")" && pwd -P)
p=fastgit-$(date +%s)-$$
pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); echo "FAIL: $*" >&2; }
# is <what> <got> <want>
is() { if [ "$2" = "$3" ]; then ok; else bad "$1: got '$2', want '$3'"; fi; }
# has <what> <text> <part>
has() { case "$2" in *"$3"*) ok ;; *) bad "$1: '$2' has no '$3'" ;; esac; }

src=$work/source; mkdir -p $src/d
for i in 1 2 3 4 5; do printf 'line %s\n' $i > $src/f$i; done
printf 'x\n' > $src/d/g; printf 'build/\n' > $src/.gitignore
( cd $src && git init -q -b main && git add -A && git commit -q -m first )
$L init $p --from-git $src -q >/dev/null 2>&1 || { echo "init failed" >&2; exit 1; }
cd $src
$L protect main --remove >/dev/null
$L migrate --delete-old >/dev/null
$L line new fast --fast -q >/dev/null || { echo "line new --fast failed" >&2; exit 1; }
w=$LAYR_ROOT/projects/$p/lines/fast
cd $w

# The folder: a link that is not content of the line, a branch with the name of the line.
[ -L .git ] && ok || bad ".git is not a link"
is "branch" "$(git symbolic-ref --short HEAD 2>&1)" fast
is "git status of a new line" "$(git status --porcelain 2>&1)" ""
is "layr status of a new line" "$($L status --porcelain 2>&1)" ""
is "the commit of the head" "$(git rev-parse HEAD)" "$($L rev-parse --git head)"
is "ls-files leaves .git out" "$($L ls-files | grep -c '^\.git$')" 0

# git commit: the head of the line follows, with git's commit, author and message.
printf 'edit\n' >> f1; printf 'new\n' > n1
git add f1 n1 && git commit -q -m "git: edit f1, add n1" || bad "git commit"
c1=$(git rev-parse HEAD)
is "state of git's commit" "$($L rev-parse --git head)" "$c1"
is "message of the state" "$($L log -n 1 --format=%s 2>&1)" "git: edit f1, add n1"
is "author of the state" "$($L log -n 1 --format=%an 2>&1)" Dev
is "layr status after git commit" "$($L status --porcelain 2>&1)" ""
is "file of the state" "$($L show head:n1 2>&1)" new
s1=$($L rev-parse head)

# A change that git did not commit stays a change of both.
printf 'not committed\n' >> f2
printf 'more\n' >> f3; git commit -q -m "git: f3 only" f3 || bad "git commit f3"
is "layr status keeps the other change" "$($L status --porcelain 2>&1)" " M f2"
is "git status keeps the other change" "$(git status --porcelain 2>&1)" " M f2"
git checkout -q -- f2

# amend, and back to an older commit: the head follows both ways.
git commit -q --amend -m "git: f3 only, amended" || bad "git commit --amend"
is "message after amend" "$($L log -n 1 --format=%s 2>&1)" "git: f3 only, amended"
is "parent after amend" "$($L rev-parse head~1 2>&1)" "$s1"
git reset -q --hard "$c1" || bad "git reset --hard"
is "head after git reset" "$($L rev-parse head 2>&1)" "$s1"
is "file after git reset" "$(cat f3)" "line 3"
is "layr status after git reset" "$($L status --porcelain 2>&1)" ""

# layr save: git's branch follows, and git sees a clean folder.
printf 'layr side\n' >> f4
$L save -a -m "layr: edit f4" -q >/dev/null 2>&1 || bad "layr save"
is "git log after layr save" "$(git log -n 1 --format=%s 2>&1)" "layr: edit f4"
is "git parent after layr save" "$(git rev-parse HEAD~1 2>&1)" "$c1"
is "git status after layr save" "$(git status --porcelain 2>&1)" ""
is "branch after layr save" "$(git symbolic-ref --short HEAD 2>&1)" fast

# Another branch of git; undo of layr.
git checkout -q -b topic || bad "git checkout -b"
printf 'on topic\n' >> f5; git commit -q -am "git: on topic" || bad "git commit on topic"
c2=$(git rev-parse HEAD)
is "state on the other branch" "$($L rev-parse --git head)" "$c2"
$L undo >/dev/null 2>&1 || bad "layr undo"
is "git branch after layr undo" "$(git log -n 1 --format=%s 2>&1)" "layr: edit f4"
is "git branch name after layr undo" "$(git symbolic-ref --short HEAD 2>&1)" topic
git reset -q --hard "$c2" || bad "git reset to c2"
is "head after the second git reset" "$($L rev-parse --git head)" "$c2"

# A checkout to another branch, then layr undo: HEAD goes back to the branch that it came from,
# and no branch moves.
git branch -q other HEAD~1
git checkout -q other || bad "git checkout other"
is "head after git checkout" "$($L rev-parse --git head)" "$(git rev-parse other)"
$L undo >/dev/null 2>&1 || bad "layr undo after a checkout"
is "branch after the undo of a checkout" "$(git symbolic-ref --short HEAD 2>&1)" topic
is "topic after the undo of a checkout" "$(git rev-parse topic)" "$c2"
is "other after the undo of a checkout" "$(git rev-parse other)" "$(git rev-parse topic~1)"
is "head after the undo of a checkout" "$($L rev-parse --git head)" "$c2"
is "git status after the undo of a checkout" "$(git status --porcelain 2>&1)" ""
is "layr status after the undo of a checkout" "$($L status --porcelain 2>&1)" ""
is "a file after the undo of a checkout" "$(tail -1 f5)" "on topic"
git branch -q -D other

# A rebase in progress does not move the line; its end does.
git checkout -q fast && git checkout -q -b side HEAD~1 || bad "git checkout side"
printf 'side\n' > side.txt; git add side.txt; git commit -q -m "git: side" || bad "git commit side"
git rebase -q topic >/dev/null 2>&1 || bad "git rebase"
is "message after rebase" "$($L log -n 1 --format=%s 2>&1)" "git: side"
is "parent after rebase" "$($L rev-parse --git head~1 2>&1)" "$c2"
is "layr status after rebase" "$($L status --porcelain 2>&1)" ""

# The state of a commit has the tree of the commit: removed and moved files, a folder that goes,
# a new folder, a link, a mode. An export of the state equals the folder (which is clean).
git rm -q f5 && git mv d/g moved-g && rmdir d && mkdir -p a/b && printf 'deep\n' > a/b/c && ln -s f1 link && chmod +x f4 || bad "tree changes"
git add -A && git commit -q -m "git: tree changes" || bad "git commit of tree changes"
is "layr status after tree changes" "$($L status --porcelain 2>&1)" ""
$L export head $work/exported >/dev/null 2>&1 || bad "layr export"
is "tree of the state" "$(diff -r --no-dereference --exclude=.git --exclude=.layr-export.json . $work/exported 2>&1)" ""
[ ! -e $work/exported/d ] && ok || bad "the folder d stays in the state"
[ -x $work/exported/f4 ] && ok || bad "f4 of the state is not executable"
# Two commits before the line follows (no hook ran): both become states.
printf 'one\n' >> f1; git -c core.hooksPath=/dev/null commit -q -am "git: quiet one" || bad "git commit without hooks"
printf 'two\n' >> f1; git -c core.hooksPath=/dev/null commit -q -am "git: quiet two" || bad "git commit without hooks"
is "the last of two commits" "$($L log -n 1 --format=%s 2>&1)" "git: quiet two"
is "the first of two commits" "$($L log -n 1 --format=%s head~1 2>&1)" "git: quiet one"
is "file after two commits" "$($L show head:f1 2>&1 | tail -1)" two

# A merge of git: a state with two parents, and the states of both sides.
git checkout -q -b left && printf 'left\n' > left.txt && git add left.txt && git commit -q -m "git: left" || bad "git commit left"
git checkout -q side && printf 'right\n' > right.txt && git add right.txt && git commit -q -m "git: right" || bad "git commit right"
git merge -q --no-ff left -m "git: merge left" >/dev/null 2>&1 || bad "git merge"
is "message of the merge" "$($L log -n 1 --format=%s 2>&1)" "git: merge left"
is "parents of the merge" "$($L log -n 1 --format=%P 2>&1 | wc -w | tr -d ' ')" 2
is "second parent of the merge" "$($L rev-parse --git 'head^2' 2>&1)" "$(git rev-parse 'HEAD^2')"
is "files of the merge" "$($L show head:left.txt 2>&1) $($L show head:right.txt 2>&1)" "left right"
is "layr status after the merge" "$($L status --porcelain 2>&1)" ""

# A push sends the commits that git made, with their ids.
git init -q --bare $work/remote.git
$L git remote add o $work/remote.git >/dev/null 2>&1
$L git push o fast >/dev/null 2>&1 || bad "layr git push"
is "pushed commit" "$(git --git-dir=$work/remote.git rev-parse fast 2>&1)" "$(git rev-parse HEAD)"
is "pushed history" "$(git --git-dir=$work/remote.git log --format=%H fast | grep -c "^$c2$")" 1
# After a fetch, the remote refs that layr has are refs of the line's folder, and the branches
# that git made in the folder stay.
$L git fetch o >/dev/null 2>&1 || bad "layr git fetch"
is "remote ref in the folder" "$(git rev-parse -q --verify refs/remotes/o/fast 2>&1)" "$(git rev-parse HEAD)"
is "branches of git after a fetch" "$(git for-each-ref --format='%(refname:short)' refs/heads | tr '\n' ' ')" "fast left side topic "

# A new line from the fast line has git's commits; a classic line stays read only for git.
$L line new second --fast -q >/dev/null 2>&1 || bad "line new from a fast line"
is "history of the new line" "$(cd ../second && git log -n 1 --format=%s 2>&1)" "git: merge left"
is "branch of the new line" "$(cd ../second && git symbolic-ref --short HEAD 2>&1)" second
$L line new classic --classic -q >/dev/null 2>&1 || bad "line new --classic"
( cd ../classic && printf 'c\n' >> f1 && git commit -q -am "refused" >/dev/null 2>&1 ) && bad "git commit in a classic line" || ok
has "adopt in a classic line" "$(cd ../classic && $L git adopt 2>&1)" "read only"

# The view tells git which paths changed (Linux): the answer equals a full scan.
if [ "$(uname -s)" != Darwin ]; then
  has "fsmonitor setting" "$(git config core.fsmonitor 2>&1)" "fs view fsmonitor"
  printf 'watched\n' >> f1; printf 'u\n' > untracked.txt; rm f2
  a=$(git status --porcelain 2>&1)
  b=$(git -c core.fsmonitor=false -c core.untrackedCache=false status --porcelain 2>&1)
  is "status with the hook" "$a" "$b"
  has "status lists the edit" "$a" " M f1"
  git checkout -q -- f1 f2; rm untracked.txt
fi

# git moved HEAD where layr cannot follow: a protected line. The head stays, and a save refuses.
cd $w
git checkout -q fast
$L status >/dev/null 2>&1
$L protect fast >/dev/null 2>&1 || bad "layr protect"
before=$($L rev-parse head)
printf 'protected\n' >> f1; git commit -q -am "git: on a protected line" || bad "git commit on a protected line"
is "head of a protected line" "$($L rev-parse head 2>&1 | tail -1)" "$before"
has "save on a protected line" "$($L save -a -m x 2>&1)" "git moved HEAD"
$L protect fast --remove >/dev/null 2>&1
has "adopt after the protection went" "$($L git adopt 2>&1)" "head is at"
is "state after adopt" "$($L log -n 1 --format=%s 2>&1)" "git: on a protected line"

# Commits in a quick row: the hooks of git run at the same time. Each waits for the line, and no
# layr command waits forever (a hook kept the shadow of the line while it waited for the line).
limit() { perl -e 'alarm shift; exec @ARGV' "$@"; }
for i in 1 2 3 4 5 6; do
  printf 'row %s\n' $i >> f2; git commit -q -am "git: row $i" || bad "git commit $i of a row"
done
limit 120 $L status >/dev/null 2>&1 && ok || bad "layr status after commits in a row: no end in 120 s"
for _ in $(seq 120); do [ "$($L log -n 1 --format=%s 2>/dev/null)" = "git: row 6" ] && break; sleep 0.5; done
is "the line follows the last commit of the row" "$(limit 60 $L log -n 1 --format=%s 2>&1)" "git: row 6"
pgrep -f "$L git adopt --hook" >/dev/null && bad "a hook of git still runs" || ok

# The folder goes with the line.
gitdir=$(readlink .git)
cd $src
$L line delete -D fast >/dev/null 2>&1 || bad "line delete"
[ -e "$gitdir" ] && bad "the .git folder stays after line delete" || ok

if [ $fail = 0 ]; then
  echo "PASS: git in a fast line ($pass checks)"
else
  echo "FAIL: $fail of $((pass + fail)) checks; evidence in $work" >&2; exit 1
fi
