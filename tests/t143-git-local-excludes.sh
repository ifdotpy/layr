#!/bin/bash
# ci: macos overlayfs
# layr ignores what git ignores through `.git/info/exclude` and the user's `core.excludesFile`:
# `init --from-git` of a clean checkout makes no state with such files, and the lines of the project
# get the patterns in their `.git/info/exclude` (project setting `ignore.local`).
. "$(dirname "$0")/lib.sh"
B=$LAYR_ROOT
export LAYR_ROOT=$B/store
mkdir -p "$LAYR_ROOT"
# The user's git configuration with a global excludes file.
printf '.DS_Store\n' > $B/global-ignore
printf '[core]\n\texcludesFile = %s\n[user]\n\tname = t\n\temail = t@t\n' "$B/global-ignore" > $B/gitconfig
export GIT_CONFIG_GLOBAL=$B/gitconfig

git init -q $B/co
cd $B/co || exit 1
echo a > a.txt && git add a.txt && git commit -qm first && git branch -M main
printf 'local-only.log\n' >> .git/info/exclude
mkdir -p crates && echo x > .DS_Store && echo x > crates/.DS_Store && echo x > local-only.log
eq "git status is clean" "" "$(git status --porcelain)"

cd $B || exit 1
out=$("$LAYR" init co --from-git $B/co 2>&1)
rc=$?
[ $rc -eq 0 ] || printf '%s\n' "$out"
eq "init succeeds" 0 "$rc"
cd $B/co || exit 1
eq "the line's status is clean" "" "$(l status --short)"
no "no state holds a file of the global excludes" grep -q "DS_Store" <<<"$(l ls-files)"
no "no state holds a file of .git/info/exclude" grep -q "local-only.log" <<<"$(l ls-files)"
has "the project keeps the patterns" ".DS_Store" "$(l config ignore.local)"
l line new -q next main >/dev/null
N=$LAYR_ROOT/projects/co/lines/next
has "a new line's .git/info/exclude has them" ".DS_Store" "$(cat $N/.git/info/exclude 2>/dev/null)"
echo x > $N/.DS_Store
eq "a new line ignores them too" "" "$(cd $N && l status --short)"
finish
