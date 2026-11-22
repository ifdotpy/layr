#!/bin/bash
# LayrFS lines on Linux (docs/layrfs-lab.md): the views contract of views-parity.sh over a LayrFS
# mount that the service's hub serves, with states that have objects only.
# Provide LAYR (built with the feature layrfs-fuse), an isolated LAYR_ROOT, and LAYR_SOCKET of a
# root daemon for that store. Run as a non-root user.
set -euo pipefail
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?Set LAYR to the binary under test}"
: "${LAYR_ROOT:?Set an isolated LAYR_ROOT}"
: "${LAYR_SOCKET:?Set LAYR_SOCKET of the store daemon}"
unset LAYR_EXPERIMENTAL_VIEWS LAYR_VIEW_BACKEND
export LAYR_AUTHOR_NAME=LayrFS LAYR_AUTHOR_EMAIL=layrfs@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null
export TMPDIR=${TMPDIR:-$(mktemp -d)}
unset LAYR_PROJECT LAYR_LINE
work=$(mktemp -d "$TMPDIR/layr-view-layrfs.XXXXXX")
project="layrfs-$(date +%s)-$$"
mkdir -p "$work/source/sub" "$work/source/ignored"
printf 'original\n' > "$work/source/file"
printf 'remove\n' > "$work/source/sub/remove"
printf 'ignored/\n' > "$work/source/.gitignore"
ln -s file "$work/source/link"
"$LAYR" init "$project" --from "$work/source" -q
base="$LAYR_ROOT/projects/$project/lines/main"
cd "$base"
"$LAYR" protect main --remove
# Without object states a fast line cannot start: --fast fails, and a fast default gives a classic line.
if "$LAYR" line new nofast --fast 2>/dev/null; then echo 'A fast line without objects.' >&2; exit 1; fi
"$LAYR" config line.kind fast
"$LAYR" line new classic1 2> "$work/note"
grep -q 'note: a classic line' "$work/note"
! grep -q " $LAYR_ROOT/projects/$project/lines/classic1 " /proc/mounts
"$LAYR" line delete -D classic1 >/dev/null
if "$LAYR" config line.kind turbo 2>/dev/null; then echo 'line.kind took turbo.' >&2; exit 1; fi
"$LAYR" migrate --delete-old
"$LAYR" line new virtual --fast
view="$LAYR_ROOT/projects/$project/lines/virtual"
grep -qE "^layrfs-view:[^ ]* $view fuse(\.layrfs)? " /proc/mounts || { echo 'The line is not a LayrFS mount.' >&2; exit 1; }
cd "$view"
[ "$(cat file)" = original ]
[ "$(stat -c %u file)" = "$(id -u)" ]
printf 'edited\n' > file
rm sub/remove
mkdir added
printf 'new\n' > added/new
chmod 755 added/new
mkdir -p ignored
printf 'cache\n' > ignored/cache
[ "$(readlink link)" = file ]
"$LAYR" status --porcelain > "$work/status"
# A fast line has the rules of a folder line: unstaged changes, untracked files, an index.
grep -q '^ M file$' "$work/status"
grep -q '^ D sub/remove$' "$work/status"
grep -q '^?? added/' "$work/status"
! grep -q ignored "$work/status"
"$LAYR" diff > "$work/diff"
grep -q edited "$work/diff"
rg --sort path -n edited . > "$work/real-rg"
LAYR_FS_STRICT=1 "$LAYR" tool rg --sort path -n edited . > "$work/layr-rg"
cmp "$work/real-rg" "$work/layr-rg"
"$LAYR" add -A
"$LAYR" save -m 'Save all view changes'
[ "$("$LAYR" show head:file)" = edited ]
[ "$("$LAYR" show head:added/new)" = new ]
if "$LAYR" show head:sub/remove 2>/dev/null; then echo 'Deleted path returned.' >&2; exit 1; fi
if "$LAYR" show head:ignored/cache 2>/dev/null; then echo 'An ignored file was saved.' >&2; exit 1; fi
"$LAYR" status --porcelain > "$work/clean"
[ ! -s "$work/clean" ]
printf 'second\n' > file
"$LAYR" save -a -m 'Second save'
[ "$("$LAYR" show head:file)" = second ]
[ "$("$LAYR" show head~1:file)" = edited ]
printf 'unsaved\n' > file
cd "$base"
"$LAYR" line show virtual --none
[ ! -e "$view" ]
"$LAYR" line show virtual --none
"$LAYR" --project "$project" --line virtual status --porcelain > "$work/hidden-status"
grep -q file "$work/hidden-status"
"$LAYR" --project "$project" --line virtual save -a -m 'Save while hidden'
[ "$("$LAYR" show virtual:file)" = unsaved ]
"$LAYR" line show virtual --virtual
"$LAYR" line show virtual --virtual
[ "$(cat "$view/file")" = unsaved ]
[ -x "$view/added/new" ]
[ "$(readlink "$view/link")" = file ]
[ ! -e "$view/sub/remove" ]
if "$LAYR" line show virtual --folder "$work/folder"; then echo 'A LayrFS line became a folder.' >&2; exit 1; fi
"$LAYR" line new hidden
printf 'keep on delete\n' > "$LAYR_ROOT/projects/$project/lines/hidden/file"
"$LAYR" line show hidden --none
"$LAYR" line delete -D --discard hidden
deleted_state=$("$LAYR" op log --all --json -n 1 | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["op"] == "line.delete" and r["data"]["name"] == "hidden"; print(r["data"]["final_state"])')
[ "$("$LAYR" show "$deleted_state:file")" = "keep on delete" ]
"$LAYR" line delete -D --discard virtual
"$LAYR" line new classic2 --classic
! grep -q " $LAYR_ROOT/projects/$project/lines/classic2 " /proc/mounts
"$LAYR" line delete -D classic2 >/dev/null
! grep -q "$LAYR_ROOT/projects/$project/" /proc/mounts || { echo 'A mount was left.' >&2; exit 1; }
# Timing: ten LayrFS lines (the first one also starts nothing: the hub runs).
for i in 1 2 3 4 5; do
    t0=$(date +%s%N); "$LAYR" line new "t$i" --fast -q; t1=$(date +%s%N)
    t2=$(date +%s%N); "$LAYR" line delete -D "t$i" >/dev/null; t3=$(date +%s%N)
    echo "line new $(( (t1 - t0) / 1000000 )) ms, line delete $(( (t3 - t2) / 1000000 )) ms"
done
printf 'PASS: LayrFS view lifecycle, saves, metadata, ignored paths, hidden saves and deletion\n'
printf 'Evidence: %s\nProject: %s\n' "$work" "$project"
