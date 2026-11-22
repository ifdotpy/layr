#!/bin/bash
# ci: macos overlayfs
# `layr init --from-git <checkout>`: a project from a Git checkout, in place. The line of the
# current branch takes the checkout's path, its history comes from the folder (no network), the
# branches of the checkout and of its origin resolve, ignored target and node_modules folders stay
# as cold cache folders, and the checkout is kept beside. A dirty checkout and extra worktrees are refused.
. "$(dirname "$0")/lib.sh"
# The checkout must be on the store's file system and out of the store. lib.sh may mount the store folder
# (btrfs), so the store moves one folder down, and the checkouts are beside it in B.
B=$LAYR_ROOT
export LAYR_ROOT=$B/store
mkdir -p "$LAYR_ROOT"
# Use Git's canonical modes for the exact HEAD check below.
umask 022

git init -q --bare $B/origin.git
git clone -q $B/origin.git $B/co 2>/dev/null
cd $B/co || exit 1
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
no "a checkout with uncommitted changes is refused" l init co --from-git $B/co
out=$(l init co --from-git $B/co 2>&1)
has "the refusal says what to do" "commit, stash or move" "$out"
has "the refusal names the path" "a.txt" "$out"
git checkout -q a.txt
git worktree add -q $B/wt2 wip 2>/dev/null
has "an extra worktree is refused" "git worktree remove" "$(l init co --from-git $B/co 2>&1)"
git worktree remove $B/wt2
ok "no project was made by the refused calls" test ! -e "$LAYR_ROOT/projects/co"

cd $B || exit 1
# Direct: the test helper `l` expects the line main in the store, and this one moves.
out=$("$LAYR" init co --from-git $B/co 2>&1)
rc=$?
[ $rc -eq 0 ] || printf '%s\n' "$out"
eq "init succeeds" 0 "$rc"
has "it names the kept checkout" "co.before-layr" "$out"
ok "the checkout is kept beside" test -d $B/co.before-layr/.git
cd $B/co || exit 1
has "the folder is the line main" "On line main" "$(l status 2>&1)"
eq "the line starts at the Git HEAD" "$(git -C $B/co.before-layr rev-parse HEAD)" "$(l rev-parse --git head)"
eq "the working folder is clean" "" "$(l status --short)"
eq "origin is the checkout's origin" "$B/origin.git" "$(l git remote get-url origin)"
eq "a branch of origin resolves" "$(git -C $B/origin.git rev-parse feat/remote)" "$(l rev-parse --git origin/feat/remote)"
eq "an unpushed local branch resolves" "$(git -C $B/co.before-layr rev-parse wip)" "$(l rev-parse --git local/wip)"
no "target output without provenance is reset" test -e rs/target/debug/x
no "node_modules without provenance is reset" test -e web/node_modules/dep/index.js
eq "the original unknown cache remains in the backup" built "$(cat "$B/co.before-layr/rs/target/debug/x")"
mkdir -p rs/target/debug
echo rebuilt > rs/target/debug/x
l save --overlays -q
before=$(l rev-parse head)
no "a repeated import refuses the existing project" "$LAYR" init co --from-git "$B/co"
eq "a repeated import keeps the line head" "$before" "$(l rev-parse head)"
eq "a repeated import keeps the local rebuild" rebuilt "$(cat rs/target/debug/x)"
eq "a repeated import keeps the original Git history" "$(git -C "$B/origin.git" rev-parse main)" "$(git -C "$B/co.before-layr" rev-parse HEAD)"
has "rs/target is a cache overlay" "folder=rs/target" "$(l overlay list 2>&1)"
has "web/node_modules is a cache overlay" "folder=web/node_modules" "$(l overlay list 2>&1)"
l line new -q next main >/dev/null
eq "a new line takes the portable cache (the default)" rebuilt "$(cat "$LAYR_ROOT/projects/co/lines/next/rs/target/debug/x" 2>&1)"

# Git does not record group write bits. Preserve them in a child state of the imported HEAD.
umask 002
git init -q -b main "$B/group"
mkdir -p "$B/group/sub"
printf 'group writable\n' > "$B/group/sub/file"
git -C "$B/group" add .
git -C "$B/group" commit -qm 'group modes'
group_head=$(git -C "$B/group" rev-parse HEAD)
ok "import preserves modes outside Git's model" "$LAYR" init group --from-git "$B/group" -q
cd "$B/group" || exit 1
eq "the group-writable file keeps its mode" 664 "$(stat_field mode sub/file)"
eq "the group-writable directory keeps its mode" 775 "$(stat_field mode sub)"
eq "the imported Git HEAD is the parent" "$group_head" "$(l rev-parse --git head~1)"
eq "the imported working folder is clean" "" "$(l status --short)"
umask 022

# A shallow clone: its history ends at a commit whose parents the clone does not have. The line
# starts at that commit, git in the line reads the same history, and a push to the origin (which
# has the parents) sends the new commits.
git init -q --bare -b main "$B/deep.git"
git clone -q "$B/deep.git" "$B/deep" 2>/dev/null
( cd "$B/deep" && git checkout -qb main 2>/dev/null; for i in 1 2 3; do echo $i > f$i.txt; git add . && git commit -qm "deep $i"; done; git push -q origin main 2>/dev/null )
git clone -q --depth 1 "file://$B/deep.git" "$B/shallow" 2>/dev/null
shallow_head=$(git -C "$B/shallow" rev-parse HEAD)
eq "the clone is shallow" true "$(git -C "$B/shallow" rev-parse --is-shallow-repository)"
ok "init --from-git takes a shallow clone" "$LAYR" init shallow --from-git "$B/shallow" -q
cd "$B/shallow" || exit 1
eq "the head is the commit of the shallow clone" "$shallow_head" "$(l rev-parse --git head)"
eq "the history of the line ends at that commit" 1 "$(l log --oneline | wc -l | tr -d ' ')"
eq "the folder of the shallow clone is clean" "" "$(l status --short)"
eq "the files of the shallow clone" "1 2 3" "$(cat f1.txt f2.txt f3.txt | tr '\n' ' ' | sed 's/ $//')"
eq "git in the line reads the same history" 1 "$(git log --oneline 2>&1 | wc -l | tr -d ' ')"
eq "git status in the line is clean" "" "$(git status --porcelain 2>&1)"
ok "a new line on a shallow history" l line new work -q
cd "$LAYR_ROOT/projects/shallow/lines/work" || exit 1
eq "git in the new line reads the same history" "$shallow_head" "$(git rev-parse HEAD 2>&1)"
echo more > more.txt
ok "add on a shallow history" l add more.txt
ok "save on a shallow history" l save -m "on top" -q
eq "git log after the save" 2 "$(git log --oneline 2>&1 | wc -l | tr -d ' ')"
ok "push of a shallow history to its origin" l git push origin work
eq "the origin has the new commit on its whole history" 4 "$(git --git-dir="$B/deep.git" log --oneline work 2>&1 | wc -l | tr -d ' ')"
eq "the parent at the origin is the commit of the clone" "$shallow_head" "$(git --git-dir="$B/deep.git" rev-parse work~1 2>&1)"
# A later commit of the origin comes with a fetch; the history of the store stays shallow.
( cd "$B/deep" && echo 4 > f4.txt && git add . && git commit -qm "deep 4" && git push -q origin main 2>/dev/null )
ok "fetch into a shallow history" l git fetch origin
eq "the fetched branch" "deep 4" "$(l log -n 1 --format=%s origin/main 2>&1)"
eq "the fetched history ends at the same commit" 2 "$(l log --oneline origin/main 2>&1 | wc -l | tr -d ' ')"
finish
