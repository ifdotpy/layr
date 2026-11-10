#!/bin/bash
# ci: macos overlayfs
# Run as a native user with LAYR, a private LAYR_ROOT, and a Linux LAYR_SOCKET when needed.
set -euo pipefail
[ "$(id -u)" != 0 ] || { echo 'Run as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
export LAYR_EXPERIMENTAL_VIEWS=1 LAYR_AUTHOR_NAME=GitTest LAYR_AUTHOR_EMAIL=git@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset LAYR_PROJECT LAYR_LINE
work=$(mktemp -d "${TMPDIR:-/tmp}/views-git.XXXXXX")
project="views-git-$$"
mkdir -p "$work/source/sub"
printf 'first\n' > "$work/source/file"
printf 'removed\n' > "$work/source/sub/removed"
ln -s file "$work/source/link"
"$LAYR" init "$project" --from "$work/source" -q
base="$LAYR_ROOT/projects/$project/lines/main"
cd "$base"
"$LAYR" protect main --remove
"$LAYR" migrate --delete-old
"$LAYR" line new virtual --view virtual
view="$LAYR_ROOT/projects/$project/lines/virtual"
cd "$view"
if [ "$(uname -s)" = Darwin ]; then [ ! -e .git ]; fi
printf 'second\n' > file
rm sub/removed
printf '#!/bin/sh\nexit 0\n' > executable
chmod +x executable
"$LAYR" save -m 'Virtual changes' -q
state=$("$LAYR" rev-parse head)
"$LAYR" git sync -q
store="$LAYR_ROOT/projects/$project/git"
# The Linux service owns this internal store. Trust only this private fixture path for direct Git checks.
store_git() { git -c "safe.directory=$store" -C "$store" "$@"; }
commit=$(store_git rev-parse "refs/layr/states/$state")
[ "$(store_git show "$commit:file")" = second ]
[ "$(store_git show "$commit:link")" = file ]
[ "$(store_git ls-tree "$commit" executable | cut -d' ' -f1)" = 100755 ]
! store_git cat-file -e "$commit:sub/removed"
[ -z "$("$LAYR" status --porcelain)" ]
if [ "$(uname -s)" = Darwin ]; then [ ! -e .git ]; fi
# Exported content excludes an unsaved edit.
printf 'unsaved\n' > file
git init -q --bare "$work/remote.git"
"$LAYR" git remote add origin "$work/remote.git"
"$LAYR" git remote -v | grep -q origin
"$LAYR" git push --dry-run origin virtual > "$work/dry-run.log"
! git -C "$work/remote.git" show-ref --verify --quiet refs/heads/virtual
"$LAYR" git push -q origin virtual
[ "$(git -C "$work/remote.git" rev-parse virtual)" = "$commit" ]
[ "$(git -C "$work/remote.git" show virtual:file)" = second ]
[ "$(cat file)" = unsaved ]
"$LAYR" git sync -q
[ "$(store_git rev-parse "refs/layr/states/$state")" = "$commit" ]
"$LAYR" save -m 'Next virtual change' -q
if [ "$(uname -s)" = Darwin ]; then
    saved=$("$LAYR" rev-parse head)
    ! store_git show-ref --verify --quiet "refs/layr/states/$saved"
fi
"$LAYR" git push -q origin virtual
next=$(git -C "$work/remote.git" rev-parse virtual)
[ "$(git -C "$work/remote.git" rev-parse virtual^)" = "$commit" ]
[ "$(git -C "$work/remote.git" show virtual:file)" = unsaved ]
# Import an upstream change, without changing the working files or saved head.
git clone -q -b virtual "$work/remote.git" "$work/upstream"
printf 'upstream\n' > "$work/upstream/file"
git -C "$work/upstream" add file
git -C "$work/upstream" -c user.name=GitTest -c user.email=git@example.invalid commit -qm upstream
git -C "$work/upstream" push -q
before=$("$LAYR" rev-parse head)
"$LAYR" git fetch -q origin
[ "$("$LAYR" show origin/virtual:file)" = upstream ]
[ "$("$LAYR" rev-parse head)" = "$before" ]
[ "$(cat file)" = unsaved ]
# A normal push cannot replace remote commits that the line does not contain.
remote_head=$(git -C "$work/remote.git" rev-parse virtual)
if "$LAYR" git push -q origin virtual > "$work/push-refused.log" 2>&1; then exit 1; fi
[ "$(git -C "$work/remote.git" rev-parse virtual)" = "$remote_head" ]
[ "$("$LAYR" rev-parse head)" = "$before" ]
# Pull, merge, and replay have a separate contract in views-git-pull.sh.
cd "$base"
"$LAYR" line show virtual --none
"$LAYR" --project "$project" --line virtual git sync -q
"$LAYR" --project "$project" --line virtual git push -q origin hidden
[ "$(git -C "$work/remote.git" rev-parse hidden)" = "$next" ]
"$LAYR" --project "$project" --line virtual git fetch -q origin
"$LAYR" line show virtual --virtual
[ "$(cat "$view/file")" = unsaved ]
if [ "$(uname -s)" = Darwin ]; then [ ! -e "$view/.git" ]; fi
"$LAYR" line show virtual --folder
"$LAYR" line delete -D virtual
printf 'PASS: virtual Git commands, v2 export, history, hidden lines, and unchanged working content\n'
