#!/bin/bash
# ci: macos
# Views hold the overlays (docs/overlays.md section 1a, slice 5): the working folder of a line
# shows the overlays of its view, not of the line. A new line folder gets the default view (every
# overlay that the caller reads, in the project order), `overlay order` sets that order,
# `overlay use` changes the view of one folder, `view <line> --overlay` makes another view of the
# same line (and `--folder` gives it a folder), a save keeps the parts of overlays that the folder
# does not show, and a line from before views keeps its stack.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true

shown() { l overlay use | awk '{print $1}' | paste -sd' ' -; }
overlays() { python3 -c 'import json,sys; print(" ".join(l["overlay"] for l in json.load(sys.stdin)["view"]["layers"]))'; }

# 1. Two overlays with a file each, saved from main.
ok "an overlay notes" l overlay create notes --mode follow -q
ok "an overlay keys" l overlay create keys --mode follow -q
eq "the project order is the order of creation" "notes keys" "$(l overlay order | paste -sd' ' -)"
ok "main shows both" l overlay use notes keys -q
echo n1 > notes.md
echo k1 > keys.txt
l overlay move notes notes.md >/dev/null
l overlay move keys keys.txt >/dev/null
echo s1 > s1.txt
l add s1.txt >/dev/null
ok "a save of the shared set and both overlays" l save -qm "one"
h1=$(l rev-parse head)

# 2. A new line gets the default view: every overlay that the caller reads, in the project order.
ok "the project order changes" l overlay order keys,notes
eq "overlay order shows it" "keys notes" "$(l overlay order | paste -sd' ' -)"
ok "a line from main" l line new -q a main
cd "$P/lines/a" || exit 1
eq "the new folder shows every overlay in the project order" "keys notes" "$(shown)"
eq "with the file of each overlay" "n1 k1" "$(cat notes.md) $(cat keys.txt)"
cd "$P/lines/main" || exit 1
eq "main keeps its own view" "notes keys" "$(shown)"

# An overlay that main does not show still comes to a new line folder.
ok "main stops to show keys" l overlay use --remove keys -q
eq "main shows notes" "notes" "$(shown)"
no "the file of keys left the folder of main" test -e keys.txt
ok "another line from main" l line new -q b main
cd "$P/lines/b" || exit 1
eq "it shows both overlays" "keys notes" "$(shown)"
eq "with the part of main on keys" k1 "$(cat keys.txt)"

# 3. Two folders of one line show different overlays: the line folder and a view folder.
cd "$P/lines/main" || exit 1
eq "view of main with --overlay names those overlays" "keys" "$(l view main --overlay keys --json | overlays)"
eq "view of main without --overlay is the view of its folder" "notes" "$(l view main --json | overlays)"
ok "a folder of the view of main with keys only" l view main --overlay keys --folder "$T/main-keys"
eq "the view folder has the file of keys" k1 "$(cat "$T/main-keys/keys.txt")"
no "and not the file of notes" test -e "$T/main-keys/notes.md"
eq "the view folder has the shared files" s1 "$(cat "$T/main-keys/s1.txt")"
eq "the line folder has notes" n1 "$(cat notes.md)"
no "and not keys" test -e keys.txt
eq "view of a state with --overlay" "notes keys" "$(l view "$h1" --overlay notes,keys --json | overlays)"
no "an unknown overlay is refused" l view main --overlay nosuch

# 4. A save in a folder that does not show an overlay keeps the line's part on that overlay.
echo s2 > s2.txt
l add s2.txt >/dev/null
ok "a save of main without keys" l save -qm "two"
ok "main shows keys again" l overlay use --add keys -q
eq "keys has the part of the line" k1 "$(cat keys.txt)"
eq "main shows both again" "notes keys" "$(shown)"

# 5. A line from before views: no view setting, only its stack. It keeps showing its stack.
cd "$P/lines/a" || exit 1
id_a=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"]=="a"))')
ok "the view of a goes (as a line made before views)" l config --unset "view.$id_a"
eq "line a shows its stack" "keys notes" "$(shown)"
echo k2 > keys.txt
ok "a save in the old line" l save -qam "keys 2"
eq "the save keeps the stack" "keys notes" "$(shown)"
ok "overlay use gives the old line a view" l overlay use notes -q
eq "line a shows notes" "notes" "$(shown)"
no "keys left its folder" test -e keys.txt

finish
