#!/bin/bash
# ci: macos overlayfs
# Exact identity deletion never follows a replacement name or working folder.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init del --no-objects --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/del/lines
# Object-backed comparisons also run on unprivileged btrfs, where metadata send is forbidden.
# Hide main during migration to avoid a native automatic-save comparison (as in t257).
python3 - "$T" "$LAYR" <<'PY_FIXTURE'
import errno
import os
from pathlib import Path
import subprocess
import sys

root, binary = Path(sys.argv[1]), sys.argv[2]
project = Path(os.environ['LAYR_ROOT']) / 'projects/del'
work = project / 'lines/main'
def run(*args):
    subprocess.run([binary, '--project', 'del', *args], cwd=root, check=True)
try:
    work.rename(root / 'main')
    hidden = True
except OSError as e:
    if e.errno not in (errno.EXDEV, errno.EBUSY):
        raise
    hidden = False
try:
    run('migrate')
    if os.environ.get('LAYR_TEST_PREMOUNTED') == '1':
        for native in (project / 'states').iterdir():
            subprocess.run(['btrfs', 'property', 'set', '-ts', str(native), 'ro', 'false'], check=True)
    run('migrate', '--delete-old')
finally:
    if hidden:
        (root / 'main').rename(work)
PY_FIXTURE
[ "$?" = 0 ] || exit 1
cd "$P/main" || exit 1

new_line() {
    l line new "$1" main --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'
}
wait_ready() {
    for i in $(seq 1 1000); do
        [ ! -e "$B/ready" ] || return 0
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.01
    done
    echo "delete did not reach its barrier" >&2
    touch "$B/release"
    wait "$pid" || true
    exit 1
}

# Resolve an id, then remove that line and recreate its name before deletion resumes.
id=$(new_line probe)
if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    eq "the deletion target is an actual OverlayFS mount" overlay "$(findmnt -rn -M "$P/probe" -o FSTYPE)"
fi
B=$T/identity; mkdir "$B"
LAYR_TEST_LINE_DELETE_BARRIER=$B l line delete --id "$id" -D --discard --json >"$T/out" 2>"$T/err" &
pid=$!
wait_ready
ok "delete the original identity" l line delete --id "$id" -D --discard
if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    no "identity deletion unmounts OverlayFS" findmnt -rn -M "$P/probe"
fi
replacement=$(new_line probe)
printf 'replacement\n' > "$P/probe/keep.txt"
touch "$B/release"
wait "$pid"; rc=$?
eq "a deleted identity is refused" 128 "$rc"
has "the refusal names the id" "$id" "$(cat "$T/err")"
has "the refusal explains the missing identity" "no longer exists" "$(cat "$T/err")"
eq "no success JSON on refusal" "" "$(cat "$T/out")"
eq "the replacement folder survives" replacement "$(cat "$P/probe/keep.txt")"
has "the replacement line survives" "$replacement" "$(l line list --json)"

# A UUID-shaped name must not shadow the exact id, even when the old row still exists.
shadow=$(new_line "$id")
no "an id never falls back to a UUID-shaped name" l line delete --id "$id" -D --discard
ok "the UUID-named line survives" test -d "$P/$id"

# Name-based deletion captures the same guard once and refuses a replacement of the same name.
id2=$(new_line named)
B=$T/name; mkdir "$B"
LAYR_TEST_LINE_DELETE_BARRIER=$B l line delete named -D --discard >"$T/out" 2>"$T/err" &
pid=$!
wait_ready
l line delete --id "$id2" -D --discard >/dev/null
new_line named >/dev/null
printf 'named replacement\n' > "$P/named/keep.txt"
touch "$B/release"
wait "$pid"; rc=$?
eq "name-based deletion refuses the replaced identity" 128 "$rc"
eq "the replacement of a resolved name survives" "named replacement" "$(cat "$P/named/keep.txt")"

# Keep the catalog identity but move its folder and put a foreign folder at its old path.
folder_id=$(new_line folder)
B=$T/folder; mkdir "$B"
LAYR_TEST_LINE_DELETE_BARRIER=$B l line delete --id "$folder_id" -D --discard >"$T/out" 2>"$T/err" &
pid=$!
wait_ready
l line move folder "$LAYR_ROOT/original-folder" >/dev/null || exit 1  # within the store file system
mkdir "$P/folder"
printf 'foreign\n' > "$P/folder/keep.txt"
touch "$B/release"
wait "$pid"; rc=$?
eq "a replacement folder is refused" 128 "$rc"
has "the refusal explains the changed folder" "working folder of line id $folder_id changed" "$(cat "$T/err")"
eq "the foreign folder survives" foreign "$(cat "$P/folder/keep.txt")"
has "the catalog line survives" "$folder_id" "$(l line list --json)"
rm "$P/folder/keep.txt"; rmdir "$P/folder"
l line move folder >/dev/null || exit 1

# A renamed line still deletes by identity, and success JSON identifies exactly that line.
l line rename folder renamed >/dev/null
out=$(l line delete --id "$folder_id" -D --discard --json)
eq "success JSON includes the exact id" "$folder_id" "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
eq "success JSON preserves the removed folder path" "$P/folder" "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["path"])')"
eq "success JSON includes the current name" renamed "$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["name"])')"
no "the renamed line's kept folder is removed" test -e "$P/folder"
no "repeated identity deletion refuses" l line delete --id "$folder_id" -D --discard
no "a partial id is refused" l line delete --id "${shadow:0:7}" -D --discard
no "a name and --id cannot be combined" l line delete probe --id "$replacement" -D --discard
no "--id is exclusive to delete" l line list --id "$replacement"

finish
