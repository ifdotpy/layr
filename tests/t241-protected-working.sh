#!/bin/bash
# ci: macos overlayfs
# Protection is about the history of a line, not its working folder. In a protected line,
# `restore`, `clean` and a `reset` to the head undo working changes; a save, and a reset that moves
# the head, still refuse.
. "$(dirname "$0")/lib.sh"
# Root bypasses the protection of a line (tests/lib.sh), so the refusals need another user.
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init prot --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/prot/lines
cd "$P/main" || exit 1
printf 'second\n' > second.txt && l add second.txt && l save -qm second
head=$(l rev-parse head)
l protect main >/dev/null
orig=$(cat notes.txt)

printf 'edit\n' >> notes.txt
no "a save in the protected line refuses" l save -a -qm edit
ok "restore in the protected line" l restore notes.txt
eq "restore brings the saved file back" "$orig" "$(cat notes.txt)"

printf 'new\n' > stray.txt
ok "clean -f in the protected line" l clean -f
no "clean removes the new file" test -e stray.txt

printf 'edit\n' >> notes.txt
rm second.txt
ok "reset --hard to the head in the protected line" l reset --hard
eq "reset --hard brings the modified file back" "$orig" "$(cat notes.txt)"
eq "reset --hard brings the deleted file back" "second" "$(cat second.txt)"
printf 'edit\n' >> notes.txt
ok "reset --hard head in the protected line" l reset --hard head
eq "reset --hard head brings the file back" "$orig" "$(cat notes.txt)"
ok "reset (mixed) to the head in the protected line" l reset
eq "the head stays" "$head" "$(l rev-parse head)"

out=$(l reset --hard head~1 2>&1) && rc=0 || rc=1
eq "a reset that moves the head back refuses" 1 "$rc"
has "the refusal names the protection" "protected" "$out"
eq "the refused reset keeps the head" "$head" "$(l rev-parse head)"
eq "the refused reset keeps the file" "second" "$(cat second.txt)"
l line new -q ahead main
cd "$P/ahead" || exit 1
printf 'ahead\n' > ahead.txt && l add ahead.txt && l save -qm ahead
cd "$P/main" || exit 1
no "a reset that moves the head forward refuses" l reset --hard ahead
eq "the head stays after the refused forward reset" "$head" "$(l rev-parse head)"

finish
