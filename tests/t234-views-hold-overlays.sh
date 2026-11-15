#!/bin/bash
# ci: macos
# Views hold the overlays (docs/overlays.md section 1, owner decision 2026-11-15): overlays form a tree
# with `main` at the root, and a view is a line seen from one overlay (the path from `main` to it).
# `overlay use` takes one overlay, a new line folder takes the view of the folder where `line new`
# ran, `view <line> --overlay` makes another view of the same line (and `--folder` gives it a
# folder), a save keeps the parts of overlays that the folder does not show, an overlay of older
# layr takes the overlay below it in the old project order as its parent, and a line of layr before
# 0.44 shows its old stack until `overlay migrate-parts`.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true

shown() { l overlay use | grep -v 'shows no overlays' | awk '{print $1}' | paste -sd' ' -; }
overlays() { python3 -c 'import json,sys; print(" ".join(l["overlay"] for l in json.load(sys.stdin)["view"]["layers"]))'; }

# 1. A tree: notes on main, keys on notes, side on main.
ok "an overlay notes" l overlay create notes -q
ok "an overlay keys above notes" l overlay create keys --parent notes -q
ok "an overlay side on main" l overlay create side -q
eq "the parent is recorded" notes "$(l config overlay.keys.parent)"
eq "main is the default parent" main "$(l config overlay.notes.parent)"
has "main is no overlay name" "bottom overlay" "$(l overlay create main 2>&1)"
has "a parent must exist" "no overlay 'nope'" "$(l overlay create x1 --parent nope 2>&1)"
has "a loop is refused" "loop" "$(l overlay set notes --parent keys 2>&1)"
eq "overlay order is gone (exit 2)" 2 "$(l overlay order >/dev/null 2>&1; echo $?)"
has "and names --parent" "--parent" "$(l overlay order 2>&1)"
ok "main shows keys" l overlay use keys -q
eq "the view of keys is the path main, notes, keys" "notes keys" "$(shown)"
eq "use takes one overlay (exit 2)" 2 "$(l overlay use notes keys >/dev/null 2>&1; echo $?)"
eq "use --add is gone (exit 2)" 2 "$(l overlay use --add side >/dev/null 2>&1; echo $?)"
echo n1 > notes.md
echo k1 > keys.txt
l overlay move notes notes.md >/dev/null
l overlay move keys keys.txt >/dev/null
echo s1 > s1.txt
l add s1.txt >/dev/null
ok "a save of the shared set and both overlays" l save -qm "one" --to main
h1=$(l rev-parse head)
eq "notes holds its file" n1 "$(l overlay show notes head:notes.md)"
eq "keys holds its file" k1 "$(l overlay show keys head:keys.txt)"

# 2. A new line takes the view of the folder where `line new` ran.
ok "a line from main, made in the folder of main" l line new -q a main
cd "$P/lines/a" || exit 1
eq "the new folder shows the view of that folder" "notes keys" "$(shown)"
eq "with the file of each overlay" "n1 k1" "$(cat notes.md) $(cat keys.txt)"
ok "this folder shows main alone" l overlay use main -q
eq "it shows no overlay" "" "$(shown)"
no "the overlay files left the folder" test -e notes.md
ok "a line made here" l line new -q b main
cd "$P/lines/b" || exit 1
eq "it shows main alone" "" "$(shown)"
ok "use notes" l overlay use notes -q
eq "the view of notes" "notes" "$(shown)"
eq "with the part of main on notes" n1 "$(cat notes.md)"
no "and not keys" test -e keys.txt

# 3. Two folders of one line show different views: the line folder and a view folder.
cd "$P/lines/main" || exit 1
ok "main shows notes" l overlay use notes -q
no "the file of keys left the folder of main" test -e keys.txt
eq "view of main with --overlay keys is the path to keys" "notes keys" "$(l view main --overlay keys --json | overlays)"
eq "view of main without --overlay is the view of its folder" "notes" "$(l view main --json | overlays)"
eq "view --overlay main is main alone" "" "$(l view main --overlay main --json | overlays)"
eq "view --overlay base is gone (exit 2)" 2 "$(l view main --overlay base >/dev/null 2>&1; echo $?)"
eq "view --overlay takes one overlay (exit 2)" 2 "$(l view main --overlay notes,keys >/dev/null 2>&1; echo $?)"
ok "a folder of the view of main seen from side" l view main --overlay side --folder "$T/main-side"
no "the view folder has no file of notes" test -e "$T/main-side/notes.md"
eq "the view folder has the shared files" s1 "$(cat "$T/main-side/s1.txt")"
ok "a folder of the view of keys" l view main --overlay keys --folder "$T/main-keys"
eq "it has the file of keys" k1 "$(cat "$T/main-keys/keys.txt")"
eq "and of notes below it" n1 "$(cat "$T/main-keys/notes.md")"
eq "view of a state with --overlay" "notes keys" "$(l view "$h1" --overlay keys --json | overlays)"
no "an unknown overlay is refused" l view main --overlay nosuch

# 4. A save in a folder that does not show an overlay keeps the line's part on that overlay.
echo s2 > s2.txt
l add s2.txt >/dev/null
ok "a save of main without keys" l save -qm "two"
ok "main shows keys again" l overlay use keys -q
eq "keys has the part of the line" k1 "$(cat keys.txt)"
eq "main shows the path again" "notes keys" "$(shown)"

# 5. A new parent: refused while a folder of this machine shows the overlay.
has "set --parent refuses while a line shows it" "shows overlay 'keys'" "$(l overlay set keys --parent side 2>&1)"
eq "the parent did not change" notes "$(l config overlay.keys.parent)"

# 6. Overlays of older layr have no parent. In the view of a folder each stands on the overlay below
# it, so an old folder keeps its content; elsewhere (and for `overlay migrate-parts`, which records
# it) the parent is the overlay below it in the old project order (the order setting, then creation).
ok "an old order" l config overlays.order '["notes","side","keys"]'
for o in notes keys side; do l config --unset "overlay.$o.parent" >/dev/null; done
eq "keys takes side, the overlay below it in the old order" "notes side keys" "$(l view main --overlay keys --json | overlays)"
id_main=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"]=="main"))')
ok "an old view of three overlays" l config "view.$id_main" '["notes","side","keys"]'
eq "it keeps its three overlays" "notes side keys" "$(shown)"
ok "an old view with a gap" l config "view.$id_main" '["notes","keys"]'
eq "keeps its content: in a view, an overlay without a parent stands on the one below it" "notes keys" "$(shown)"
ok "overlay migrate-parts records the parents of the old order" l overlay migrate-parts -q
eq "keys has side as its parent now" side "$(l config overlay.keys.parent)"
eq "and the view of keys is the path of the old order" "notes side keys" "$(shown)"
l config overlay.keys.parent '"notes"' >/dev/null
l config overlay.notes.parent '"main"' >/dev/null
l config overlay.side.parent '"main"' >/dev/null
ok "main shows keys" l config "view.$id_main" '["notes","keys"]'
eq "the recorded parents win again" "notes keys" "$(shown)"

# 7. A line of layr before 0.44: no view, only its old stack (`stack.<line id>`). It shows its stack,
# and `overlay migrate-parts` gives it a view and its lines on the overlays.
cd "$P/lines/a" || exit 1
ok "a shows keys" l overlay use keys -q
id_a=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"]=="a"))')
stack=$(l view a --json | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["view"]["layers"]))')
ok "the old stack of a" l config "stack.$id_a" "$stack"
ok "the view of a goes (as a line made before views)" l config --unset "view.$id_a"
eq "line a shows its stack" "notes keys" "$(shown)"
ok "the migration" l overlay migrate-parts -q
eq "a has a view now" "notes keys" "$(l config "view.$id_a" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)))')"
echo k2 > keys.txt
ok "a save in the old line" l save -qam "keys 2"
eq "the save keeps the overlays" "notes keys" "$(shown)"
ok "overlay use changes the view" l overlay use notes -q
eq "line a shows notes" "notes" "$(shown)"
no "keys left its folder" test -e keys.txt

# 8. Readers rule (docs/overlays.md section 1): a reader of an overlay reads every overlay below it.
has "a reader of keys who does not read notes is refused" "but not its ancestor 'notes'" "$(l overlay create k2 --parent notes --readers uid:1 2>&1)"
no "the overlay was not made" l overlay show k2 --json
ok "notes gets the reader" l overlay readers notes uid:1
ok "then keys can have it" l overlay readers keys uid:1
has "notes cannot drop a reader that keys has" "which is above 'notes'" "$(l overlay readers notes "$(id -un)" 2>&1)"
has "a public overlay cannot be above a hidden one" "hidden" "$(l overlay create hid7 --hidden -q; l overlay create pub7 --parent hid7 2>&1)"

finish
