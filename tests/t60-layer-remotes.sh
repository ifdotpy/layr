#!/bin/bash
# Remotes per layer (docs/cache-overlays.md, section 7): the shared set goes to the project's git
# remotes, an overlay to its own git remote, a cache never to git; `layr push` sends every layer,
# `--dry-run` lists them. One user, local bare repositories, so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
git init -q --bare $T/shared.git
git init -q --bare $T/notes.git
l init p --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main
l protect main --remove >/dev/null 2>&1 || true
l git remote add origin $T/shared.git
echo n1 > notes.md
l overlay create notes --mode follow -q
l overlay move notes notes.md >/dev/null
l save -m notes -q
rm -rf target; mkdir target; echo bin > target/out
l save --caches -q

ok "an overlay git remote" l overlay remote add notes notes-gh $T/notes.git
eq "it is listed" "notes-gh	$T/notes.git" "$(l overlay remote list notes)"
no "a cache never goes to git" l overlay remote add cache.target c $T/notes.git
no "a remote needs a URL" l overlay remote add notes broken

out=$(l push --dry-run)
has "dry-run lists the shared set" "shared	origin	git	" "$out"
has "and the overlay" "notes	notes-gh	git	" "$out"
eq "and no cache row without a layr remote" "" "$(echo "$out" | grep cache)"
eq "dry-run sends nothing" "" "$(git --git-dir=$T/shared.git branch)"

ok "layr push sends every layer" l push -q
eq "the shared set has no overlay file" "" "$(git --git-dir=$T/shared.git ls-tree -r --name-only main | grep -E 'notes.md|target')"
has "the shared set is there" "notes.txt" "$(git --git-dir=$T/shared.git ls-tree -r --name-only main)"
eq "the overlay repository has only the overlay files" "notes.md" "$(git --git-dir=$T/notes.git ls-tree -r --name-only main)"
eq "with the overlay content" n1 "$(git --git-dir=$T/notes.git show main:notes.md)"
has "a second dry-run has nothing new" "notes	notes-gh	git	0 new state(s)" "$(l push --dry-run)"

echo n2 > notes.md
l save -a -m "notes 2" -q
ok "the next push" l push -q
eq "the overlay branch moved" n2 "$(git --git-dir=$T/notes.git show main:notes.md)"
eq "the overlay history has both states" 3 "$(git --git-dir=$T/notes.git rev-list --count main)"

# A layr remote takes the whole repository, the cache too.
l remote add archive "file://$T/archive" --paper-key-out $T/paper.txt >/dev/null 2>&1
out=$(l push --dry-run)
has "a layr remote takes the repository" "repository	archive	layr" "$out"
has "and the cache with its platform" "cache.target (target)	archive	layr	1 state" "$out"
ok "overlay remote remove" l overlay remote remove notes notes-gh
eq "it is gone" "" "$(l overlay remote list notes)"

finish
