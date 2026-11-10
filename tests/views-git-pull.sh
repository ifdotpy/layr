#!/bin/bash
# ci: macos overlayfs
# Native-user Git pull contract for FSKit, Btrfs, and OverlayFS with object store v2.
set -euo pipefail
[ "$(id -u)" != 0 ] || { echo 'Run as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
export LAYR_EXPERIMENTAL_VIEWS=1 LAYR_AUTHOR_NAME=PullTest LAYR_AUTHOR_EMAIL=pull@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=PullTest GIT_AUTHOR_EMAIL=pull@example.invalid
export GIT_COMMITTER_NAME=PullTest GIT_COMMITTER_EMAIL=pull@example.invalid
unset LAYR_PROJECT LAYR_LINE
work=$(mktemp -d "${TMPDIR:-/tmp}/vgp.XXXXXX")
project="views-pull-$$"
cleanup() {
    cd "$work"
    for name in virtual replay hidden; do
        "$LAYR" --project "$project" line show "$name" --none >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT
R="$work/remote.git"
U="$work/upstream"
git init -q --bare -b main "$R"
git clone -q "$R" "$U" 2>/dev/null
printf 'initial\n' > "$U/file"
printf 'delete\n' > "$U/removed"
printf 'base\n' > "$U/other"
ln -s file "$U/link"
git -C "$U" add -A
git -C "$U" commit -qm initial
git -C "$U" push -q origin main
cd "$work"
"$LAYR" git clone "$R" "$project" -q
base="$LAYR_ROOT/projects/$project/lines/main"
cd "$base"
"$LAYR" migrate --delete-old
"$LAYR" line new virtual --view virtual
view="$base/../virtual"
cd "$view"
# Fast-forward updates the mount, with deletion, symlink and executable mode.
printf 'upstream\n' > "$U/file"
rm "$U/removed" "$U/link"
printf '#!/bin/sh\nexit 0\n' > "$U/executable"
chmod +x "$U/executable"
ln -s executable "$U/link"
git -C "$U" add -A
git -C "$U" commit -qm upstream
git -C "$U" push -q
printf 'local unsaved\n' > local-note
"$LAYR" git pull --ff-only origin main
[ "$(cat file)" = upstream ]
[ ! -e removed ] && [ -x executable ]
[ "$(readlink link)" = executable ]
[ "$(cat local-note)" = 'local unsaved' ]
"$LAYR" save -m 'Keep local note' -q
# Refuse an update that would overwrite an unsaved file.
printf 'dirty\n' > file
printf 'remote edit\n' > "$U/file"
git -C "$U" commit -qam remote-edit
git -C "$U" push -q
head=$("$LAYR" rev-parse head)
if "$LAYR" git pull origin main > "$work/dirty.log" 2>&1; then exit 1; fi
grep -q 'local changes' "$work/dirty.log"
[ "$(cat file)" = dirty ]
[ "$("$LAYR" rev-parse head)" = "$head" ]
printf 'upstream\n' > file
# The local commit requires a merge; ff-only must leave the head unchanged.
if "$LAYR" git pull --ff-only origin main > "$work/ff.log" 2>&1; then exit 1; fi
grep -q 'Not possible to fast-forward' "$work/ff.log"
[ "$("$LAYR" rev-parse head)" = "$head" ]
# An outdated review expectation must refuse before changing the working files.
if "$LAYR" git pull --expect "$head" origin main > "$work/expect.log" 2>&1; then exit 1; fi
grep -q -- '--expect' "$work/expect.log"
[ "$("$LAYR" rev-parse head)" = "$head" ]
[ "$(cat file)" = upstream ]
expected=$("$LAYR" rev-parse origin/main)
"$LAYR" git pull --expect "$expected" origin main
[ "$(cat file)" = 'remote edit' ]
[ "$(cat local-note)" = 'local unsaved' ]
[ "$("$LAYR" log -n 1 --format=%p | wc -w | tr -d ' ')" = 2 ]
[ -z "$("$LAYR" status --porcelain)" ]
# A conflict remains visible, and saving edited markers is refused.
printf 'local conflict\n' > file
"$LAYR" save -m 'Local conflict' -q
printf 'remote conflict\n' > "$U/file"
git -C "$U" commit -qam remote-conflict
git -C "$U" push -q
if "$LAYR" git pull origin main > "$work/conflict.log" 2>&1; then exit 1; fi
grep -q '^<<<<<<<' file
"$LAYR" status --json | python3 -c 'import json,sys; assert any(c["path"]=="file" for c in json.load(sys.stdin)["conflicts"])'
head=$("$LAYR" rev-parse head)
printf 'more text\n' >> file
if "$LAYR" save -m 'Unresolved' > "$work/markers.log" 2>&1; then exit 1; fi
grep -q 'conflict markers' "$work/markers.log"
[ "$("$LAYR" rev-parse head)" = "$head" ]
printf 'resolved\n' > file
"$LAYR" save -m 'Resolve conflict' -q
"$LAYR" status --json | python3 -c 'import json,sys; assert not json.load(sys.stdin)["conflicts"]'
# Exported v2 states remain the same states when fetched back, so ff-only works.
"$LAYR" git push -q origin roundtrip
git -C "$U" fetch -q origin roundtrip
git -C "$U" checkout -qb roundtrip origin/roundtrip
printf 'roundtrip\n' > "$U/roundtrip"
git -C "$U" add roundtrip
git -C "$U" commit -qm roundtrip
git -C "$U" push -q origin roundtrip
"$LAYR" git pull --ff-only origin roundtrip
[ "$(cat roundtrip)" = roundtrip ]
[ -z "$("$LAYR" status --porcelain)" ]
# Replay one local commit onto a new upstream state.
git -C "$U" checkout -q main
cd "$base"
"$LAYR" line new replay origin/main --view virtual
cd ../replay
printf 'replayed\n' > replayed
"$LAYR" save -m 'Replay this commit' -q
printf 'remote rebase\n' > "$U/other"
git -C "$U" commit -qam remote-rebase
git -C "$U" push -q
head=$("$LAYR" rev-parse head)
if "$LAYR" git pull --rebase --expect "$head" origin main > "$work/replay-expect.log" 2>&1; then exit 1; fi
grep -q -- '--expect' "$work/replay-expect.log"
[ "$("$LAYR" rev-parse head)" = "$head" ]
[ "$(cat other)" = base ]
[ "$(cat replayed)" = replayed ]
expected=$("$LAYR" rev-parse origin/main)
"$LAYR" git pull --rebase --expect "$expected" origin main
[ "$(cat replayed)" = replayed ]
[ "$(cat other)" = 'remote rebase' ]
[ "$("$LAYR" log -n 1 --format=%s)" = 'Replay this commit' ]
[ "$("$LAYR" log -n 1 --format=%p | wc -w | tr -d ' ')" = 1 ]
[ -z "$("$LAYR" status --porcelain)" ]
# The short replay flag has the same meaning.
printf 'second replay\n' > replayed
"$LAYR" save -m 'Replay alias' -q
printf 'alias upstream\n' > "$U/other"
git -C "$U" commit -qam alias-upstream
git -C "$U" push -q
"$LAYR" git pull -r origin main
[ "$(cat replayed)" = 'second replay' ]
[ "$(cat other)" = 'alias upstream' ]
[ "$("$LAYR" log -n 1 --format=%s)" = 'Replay alias' ]
[ "$("$LAYR" log -n 1 --format=%p | wc -w | tr -d ' ')" = 1 ]
# Hidden pulls restore the hidden view after success and failure.
cd "$base"
"$LAYR" line new hidden origin/main --view virtual
"$LAYR" line show hidden --none
printf 'hidden update\n' > "$U/hidden"
git -C "$U" add hidden
git -C "$U" commit -qm hidden
git -C "$U" push -q
"$LAYR" --project "$project" --line hidden git pull --ff-only origin main
[ ! -e "$base/../hidden" ]
[ "$("$LAYR" show hidden:hidden)" = 'hidden update' ]
if "$LAYR" --project "$project" --line hidden git pull missing main > "$work/missing.log" 2>&1; then exit 1; fi
[ ! -e "$base/../hidden" ]
"$LAYR" line show hidden --virtual
[ "$(cat "$base/../hidden/hidden")" = 'hidden update' ]
if [ "$(uname -s)" = Darwin ]; then [ ! -e "$base/../hidden/.git" ]; fi
printf 'PASS: virtual Git pull, ff-only, merge, conflicts, replay, v2 roundtrip, and hidden views\n'
