#!/bin/bash
# ci: macos overlayfs
# `layr init --from-git <checkout>`: a project from a Git checkout, in place. The line of the
# current branch takes the checkout's path, its history comes from the folder (no network), the
# branches of the checkout and of its origin resolve, ignored target and node_modules folders stay
# as caches, and the checkout is kept beside. A dirty checkout and extra worktrees are refused.
. "$(dirname "$0")/lib.sh"

git init -q --bare $T/origin.git
git clone -q $T/origin.git $T/co 2>/dev/null
cd $T/co || exit 1
mkdir -p rs web && printf '[package]\nname = "x"\n' > rs/Cargo.toml && echo '{}' > web/package.json
printf 'target/\nnode_modules/\n' > .gitignore
echo a > a.txt
git add . && git commit -qm first && git branch -M main && git push -q origin main 2>/dev/null
git checkout -qb feat/remote && echo r > r.txt && git add r.txt && git commit -qm remote && git push -q origin feat/remote 2>/dev/null
git checkout -q main && git branch -qD feat/remote
git checkout -qb wip && echo w > w.txt && git add w.txt && git commit -qm unpushed
git checkout -q main
mkdir -p rs/target/debug web/node_modules/dep
echo built > rs/target/debug/x; echo dep > web/node_modules/dep/index.js

# Refused: uncommitted changes, then an extra worktree.
echo dirty >> a.txt
no "a checkout with uncommitted changes is refused" l init co --from-git $T/co
out=$(l init co --from-git $T/co 2>&1)
has "the refusal says what to do" "commit, stash or move" "$out"
has "the refusal names the path" "a.txt" "$out"
git checkout -q a.txt
git worktree add -q $T/wt2 wip 2>/dev/null
has "an extra worktree is refused" "git worktree remove" "$(l init co --from-git $T/co 2>&1)"
git worktree remove $T/wt2
ok "no project was made by the refused calls" test ! -e "$LAYR_ROOT/projects/co"

cd $T || exit 1
# Direct: the test helper `l` expects the line main in the store, and this one moves.
out=$("$LAYR" init co --from-git $T/co 2>&1)
rc=$?
[ $rc -eq 0 ] || printf '%s\n' "$out"
eq "init succeeds" 0 "$rc"
has "it names the kept checkout" "co.before-layr" "$out"
ok "the checkout is kept beside" test -d $T/co.before-layr/.git
cd $T/co || exit 1
has "the folder is the line main" "On line main" "$(l status 2>&1)"
eq "the line starts at the Git HEAD" "$(git -C $T/co.before-layr rev-parse HEAD)" "$(l rev-parse --git head)"
eq "the working folder is clean" "" "$(l status --short)"
eq "origin is the checkout's origin" "$T/origin.git" "$(l git remote get-url origin)"
eq "a branch of origin resolves" "$(git -C $T/origin.git rev-parse feat/remote)" "$(l rev-parse --git origin/feat/remote)"
eq "an unpushed local branch resolves" "$(git -C $T/co.before-layr rev-parse wip)" "$(l rev-parse --git local/wip)"
eq "the target cache is kept" built "$(cat rs/target/debug/x)"
eq "the node_modules cache is kept" dep "$(cat web/node_modules/dep/index.js)"
has "rs/target is a cache overlay" "folder=rs/target" "$(l overlay list 2>&1)"
has "web/node_modules is a cache overlay" "folder=web/node_modules" "$(l overlay list 2>&1)"
l line new -q next main >/dev/null
eq "a new line starts warm" built "$(cat $LAYR_ROOT/projects/co/lines/next/rs/target/debug/x)"
finish
