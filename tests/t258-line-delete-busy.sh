#!/bin/bash
# ci: macos overlayfs
# `line delete` of a line that a process uses as its working directory (a shell in the line).
# On OverlayFS the folder is a busy mount: the delete detaches it, as APFS deletes such a folder.
# The record goes only with the folder, so a second delete never finds an orphan folder.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init del --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/del/lines
cd "$P/main" || exit 1
l line new -q busy main || exit 1
cd "$P/busy" || exit 1
mkdir deep
printf 'saved\n' > saved.txt
l add saved.txt && l save -qm saved || exit 1
cd "$P/main" || exit 1
l merge -q busy || exit 1

# One process in a subfolder, and this shell in the line folder itself.
(cd "$P/busy/deep" && exec sleep 300) &
holder=$!
trap 'kill "$holder" 2>/dev/null' EXIT
cd "$P/busy" || exit 1
ok "a line used as a working directory is deleted" l --project del --line main line delete busy
cd "$P/main" || exit 1
no "its folder is gone" test -e "$P/busy"
out=$(l line delete busy 2>&1)
has "the record went with the folder" "line 'busy' not found" "$out"
kill "$holder" 2>/dev/null
wait "$holder" 2>/dev/null

ok "undo of the delete" l undo
eq "undo brings the saved file back" "saved" "$(cat "$P/busy/saved.txt")"
ok "the line deletes again" l line delete busy
ok "a new line takes the name" l line new -q busy main
eq "the new line has its own folder" "saved" "$(cat "$P/busy/saved.txt")"

finish
