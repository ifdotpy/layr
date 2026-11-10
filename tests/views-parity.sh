#!/bin/bash
# Native-user contract for macOS FSKit and Linux Btrfs/OverlayFS views.
# Provide LAYR, an isolated LAYR_ROOT, and on Linux a matching daemon's LAYR_SOCKET.
set -euo pipefail
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?Set LAYR to the binary under test}"
: "${LAYR_ROOT:?Set an isolated LAYR_ROOT}"
export LAYR_EXPERIMENTAL_VIEWS=1
export LAYR_AUTHOR_NAME=Parity LAYR_AUTHOR_EMAIL=parity@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null
export LAYR_FS_SOCKET="${TMPDIR:-/tmp}/layr-parity-tools-$$.sock"
trap '"$LAYR" fs stop >/dev/null 2>&1 || true' EXIT
unset LAYR_PROJECT LAYR_LINE
work=$(mktemp -d "${TMPDIR:-/tmp}/layr-view-parity.XXXXXX")
project="parity-$(date +%s)-$$"
mkdir -p "$work/source/sub" "$work/source/ignored"
printf 'original\n' > "$work/source/file"
printf 'remove\n' > "$work/source/sub/remove"
printf 'ignored/\n' > "$work/source/.gitignore"
ln -s file "$work/source/link"
"$LAYR" init "$project" --from "$work/source" -q
base="$LAYR_ROOT/projects/$project/lines/main"
cd "$base"
"$LAYR" protect main --remove
if [ "${LAYR_TEST_OBJECT_STATES:-}" = 1 ]; then
    "$LAYR" migrate --delete-old
fi
if LAYR_EXPERIMENTAL_VIEWS= "$LAYR" line new disabled --view virtual; then
    echo 'The experimental flag must be required.' >&2; exit 1
fi
"$LAYR" line new virtual --view virtual
view="$LAYR_ROOT/projects/$project/lines/virtual"
cd "$view"
[ "$(cat file)" = original ]
printf 'edited\n' > file
rm sub/remove
mkdir added
printf 'new\n' > added/new
chmod 755 added/new
mkdir -p ignored
printf 'cache\n' > ignored/cache
[ "$(readlink link)" = file ]
"$LAYR" status --porcelain > "$work/status"
grep -q '^M  file$' "$work/status"
grep -q '^D  sub/remove$' "$work/status"
grep -q '^A  added/new$' "$work/status"
! grep -q ignored/cache "$work/status"
"$LAYR" diff > "$work/diff"
grep -q edited "$work/diff"
grep -q added/new "$work/diff"
grep -q "^+new$" "$work/diff"
"$LAYR" fs warm "$view" --wait
rg --sort path -n edited file > "$work/real-rg"
LAYR_FS_STRICT=1 "$LAYR" tool rg --sort path -n edited file > "$work/layr-rg"
cmp "$work/real-rg" "$work/layr-rg"
printf 'fresh edited\n' > fresh
rg --sort path -n edited . > "$work/real-rg"
LAYR_FS_STRICT=1 "$LAYR" tool rg --sort path -n edited . > "$work/layr-rg"
cmp "$work/real-rg" "$work/layr-rg"
"$LAYR" save -m 'Save all view changes'
[ "$("$LAYR" show head:file)" = edited ]
[ "$("$LAYR" show head:added/new)" = new ]
if "$LAYR" show head:sub/remove; then echo 'Deleted path returned.' >&2; exit 1; fi
"$LAYR" status --porcelain > "$work/clean"
[ ! -s "$work/clean" ]
printf 'unsaved\n' > file
cd "$base"
"$LAYR" line show virtual --none
[ ! -e "$view" ]
"$LAYR" line show virtual --none
"$LAYR" --project "$project" --line virtual status --porcelain > "$work/hidden-status"
grep -q file "$work/hidden-status"
"$LAYR" --project "$project" --line virtual save -m 'Save while hidden'
[ "$("$LAYR" show virtual:file)" = unsaved ]
"$LAYR" line show virtual --virtual
"$LAYR" line show virtual --virtual
[ "$(cat "$view/file")" = unsaved ]
[ -x "$view/added/new" ]
[ "$(readlink "$view/link")" = file ]
[ ! -e "$view/sub/remove" ]
# Conversion must retain unsaved edits too, without overwriting an existing target.
mkdir "$work/occupied"
if "$LAYR" line show virtual --folder "$work/occupied"; then exit 1; fi
[ "$(cat "$view/file")" = unsaved ]
printf 'after hidden save\n' > "$view/file"
"$LAYR" line show virtual --folder
[ "$(cat "$view/file")" = "after hidden save" ]
cd "$view"
"$LAYR" add file
"$LAYR" save -m 'Save after conversion'
[ "$("$LAYR" show head:file)" = "after hidden save" ]
cd "$base"
"$LAYR" line new hidden --view virtual
printf 'keep on delete\n' > "$LAYR_ROOT/projects/$project/lines/hidden/file"
"$LAYR" line show hidden --none
"$LAYR" line delete -D hidden
# Read the recorded final state, then verify it through the public show command.
deleted_state=$("$LAYR" op log --all --json -n 1 | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["op"] == "line.delete" and r["data"]["name"] == "hidden"; print(r["data"]["final_state"])')
[ "$("$LAYR" show "$deleted_state:file")" = "keep on delete" ]
"$LAYR" line delete -D virtual
if [ "$(uname -s)" = Linux ]; then
    "$LAYR" line new retry --view virtual
    retry="$LAYR_ROOT/projects/$project/lines/retry"
    printf 'survive failed move\n' > "$retry/file"
    if LAYR_TEST_VIEW_MOVE_FAIL_AFTER_RENAME=1 "$LAYR" line show retry --none; then exit 1; fi
    if "$LAYR" line delete -D retry; then echo 'Delete accepted an unresolved move.' >&2; exit 1; fi
    if "$LAYR" --project "$project" --line retry save -m blocked; then exit 1; fi
    "$LAYR" line show retry --virtual
    [ "$(cat "$retry/file")" = "survive failed move" ]
    if LAYR_TEST_VIEW_MOVE_FAIL_AFTER_RENAME=1 "$LAYR" line show retry --folder "$work/moved"; then exit 1; fi
    "$LAYR" line show retry --folder "$work/moved"
    [ "$(cat "$work/moved/file")" = "survive failed move" ]
    "$LAYR" line delete -D retry
fi
printf 'PASS: %s view lifecycle, saves, metadata, ignored paths, and hidden deletion\n' "$(uname -s)"
printf 'Evidence: %s\nProject: %s\n' "$work" "$project"
