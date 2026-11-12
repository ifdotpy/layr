#!/bin/bash
# ci: macos overlayfs
# `line delete` refuses a line whose folder has unsaved changes (modified, added or new files that
# are not ignored), also with -D: -D allows only unmerged saves. `--discard` (or -f) deletes the
# line with its changes. A line without unsaved changes deletes as before.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init del --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/del/lines
cd "$P/main" || exit 1

# A new file that is not ignored.
l line new -q dirty main
cd "$P/dirty" || exit 1
printf 'keep me\n' > new.txt
cd "$P/main" || exit 1
out=$(l line delete -D dirty 2>&1); rc=$?
eq "delete -D of a line with a new file refuses" 1 "$rc"
has "the error names the line" "line 'dirty' has unsaved changes" "$out"
has "the error names the file" "new.txt" "$out"
has "the error names the discard flag" "layr line delete -D --discard dirty" "$out"
eq "the new file survives" "keep me" "$(cat "$P/dirty/new.txt")"
out=$(l line delete dirty 2>&1); rc=$?
eq "delete without -D refuses too" 1 "$rc"
has "the hint without -D" "layr line delete --discard dirty" "$out"

# A modified tracked file.
l line new -q edited main
cd "$P/edited" || exit 1
printf 'changed\n' >> notes.txt
cd "$P/main" || exit 1
no "delete -D of a line with a modified file refuses" l line delete -D edited
has "the modified file survives" "changed" "$(cat "$P/edited/notes.txt")"

# Ignored files (build output) are not unsaved changes.
l line new -q built main
cd "$P/built" || exit 1
printf 'log\n' > build.log
cd "$P/main" || exit 1
ok "delete -D of a line with ignored files only" l line delete -D built
no "the line with ignored files is deleted" test -e "$P/built"

# --discard deletes the line with its changes; undo brings them back.
ok "delete -D --discard of a line with a new file" l line delete -D --discard dirty
no "the discarded line is deleted" test -e "$P/dirty"
ok "undo of the discard" l undo
eq "undo brings the new file back" "keep me" "$(cat "$P/dirty/new.txt")"
ok "delete -f of a line with a new file" l line delete -f dirty
no "the line is deleted with -f" test -e "$P/dirty"
ok "delete --discard of a line with a modified file" l line delete --discard edited
no "the edited line is deleted" test -e "$P/edited"

# A line without unsaved changes deletes without a flag; one with unmerged saves needs -D only.
l line new -q clean main
cd "$P/clean" || exit 1
printf 'clean\n' > c.txt && l add c.txt && l save -qm clean
cd "$P/main" || exit 1
no "delete of an unmerged clean line refuses without -D" l line delete clean
ok "delete -D of a clean line" l line delete -D clean
no "the clean line is deleted" test -e "$P/clean"

finish
