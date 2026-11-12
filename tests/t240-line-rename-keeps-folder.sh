#!/bin/bash
# ci: macos overlayfs
# `line rename` gives the line a new name and keeps its folder where it is: a shell, an editor or
# an agent that works in the folder keeps a valid working folder. `line move <line>` moves the
# folder to `lines/<name>` on request, and `line new` with the old name does not take the folder.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q feat main >/dev/null
# path_of <line>: the folder that `line list --json` shows for the line.
path_of() { l line list --json | python3 -c "import json,sys; print(next(x['path'] for x in json.load(sys.stdin) if x['name']=='$1'))"; }

# A shell works in a subfolder of the line while another folder renames the line.
(cd "$P/feat/src" && while [ ! -e "$T/go" ]; do sleep 0.05; done
    { pwd -P; cat main.rs >/dev/null && echo readable; "$LAYR" status | head -1; } >"$T/shell.out" 2>&1) &
SHELL_PID=$!
cd "$P/feat" || exit 1
ok "a rename from another folder" sh -c "cd '$P/main' && '$LAYR' line rename feat feat2 >/dev/null"
touch "$T/go"; wait $SHELL_PID
eq "the waiting shell keeps its folder" "$P/feat/src" "$(head -1 "$T/shell.out")"
has "the waiting shell reads its files" "readable" "$(cat "$T/shell.out")"
has "status in the waiting shell shows the new name" "On line feat2" "$(cat "$T/shell.out")"

# This shell was in the folder too.
eq "the current folder stays valid" "$P/feat" "$(pwd -P)"
ok "the folder keeps its files" test -f src/main.rs
no "no folder of the new name" test -e "$P/feat2"
has "status shows the new name" "On line feat2" "$(l status)"
eq "the folder finds the renamed line" "feat2" "$(l line list --show-current)"
eq "line list shows the old folder" "$P/feat" "$(path_of feat2)"
eq "line list has no line of the old name" "" "$(l line list --json | python3 -c "import json,sys; print(*[x['name'] for x in json.load(sys.stdin) if x['name']=='feat'])")"
echo kept > kept.txt
ok "add in the kept folder" l add kept.txt
ok "save in the kept folder" l save -qm "in the kept folder"
has "log shows the save" "in the kept folder" "$(l log -1)"
cd src || exit 1
echo more >> main.rs
has "status from a subfolder" "main.rs" "$(l status --short)"
cd "$P/main" || exit 1
eq "the head of the renamed line" "$(cd "$P/feat" && l rev-parse head)" "$(l rev-parse feat2)"

# A new line with the old name does not take the kept folder.
out=$(l line new feat main 2>&1); rc=$?
no "line new with the old name is refused" test "$rc" = 0
has "the refusal names the line of the folder" "belongs to line 'feat2'" "$out"
has "the refusal names the move" "layr line move feat2" "$out"
ok "the kept folder is unchanged" test -f "$P/feat/kept.txt"

# The folder moves to the new name only on request.
ok "line move without a path" l line move feat2
ok "the folder has the new name" test -f "$P/feat2/kept.txt"
no "the old folder is gone" test -e "$P/feat"
eq "line list shows the folder of the new name" "$P/feat2" "$(path_of feat2)"
out=$(l line move feat2 2>&1)
has "a second move finds it in place" "already in the store" "$out"
ok "now line new takes the old name" l line new -q feat main
eq "the new line has its own folder" "$P/feat" "$(path_of feat)"
eq "the new line is not the renamed line" "feat" "$(cd "$P/feat" && l line list --show-current)"

# A rename back to the name of the folder forgets the kept folder.
ok "a rename" l line rename feat x
ok "a rename back" l line rename x feat
eq "the folder of the name again" "$P/feat" "$(path_of feat)"
has "the line is in its default folder" "already in the store" "$(l line move feat 2>&1)"

# Two renames in a row keep the first folder.
ok "rename one" l line rename feat2 a1
ok "rename two" l line rename a1 a2
eq "the first folder stays" "$P/feat2" "$(path_of a2)"
eq "the kept folder finds the last name" "a2" "$(cd "$P/feat2" && l line list --show-current)"

# A line moved out of the store keeps its path through a rename.
mkdir -p "$T/outside"
ok "move out" l line move a2 "$T/outside/w"
ok "rename a moved line" l line rename a2 a3
eq "the moved path stays" "$T/outside/w" "$(path_of a3)"
ok "move back" l line move a3
eq "back in the folder of the new name" "$P/a3" "$(path_of a3)"

# A deleted line takes its kept folder with it.
ok "rename for the delete" l line rename a3 gone
ok "delete of a renamed line" l line delete -D gone
no "its folder is gone" test -e "$P/a3"
ok "a new line takes the free name" l line new -q a3 main
ok "with its own folder" test -d "$P/a3"

finish
