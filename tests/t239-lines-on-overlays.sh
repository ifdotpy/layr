#!/bin/bash
# ci: macos
# A line is the same line on every overlay (docs/overlays.md section 1a, "Lines on overlays as built"):
# the store of an overlay holds it with the id and the name of the line, `line rename` renames it there,
# a deleted line leaves no line on the overlays, and no setting marks a build-output overlay as a cache.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true

# lines_of <overlay>: "name id" of each line of the overlay store.
lines_of() {
    sqlite3 "$P/meta/overlays/$1/meta/db/layr.sqlite" "SELECT name || ' ' || id FROM lines WHERE deleted=0 ORDER BY name"
}
id_of() { l line list --json | python3 -c "import json,sys; print(next(x['id'] for x in json.load(sys.stdin) if x['name']=='$1'))"; }

ok "an overlay notes" l overlay create notes --mode follow -q
echo n1 > notes.md
l overlay move notes notes.md >/dev/null
ok "a save" l save -qam one
eq "the overlay holds main with its name and id" "main $(id_of main)" "$(lines_of notes)"

ok "a line feat" l line new -q feat main
cd "$P/lines/feat" || exit 1
echo n2 > notes.md
ok "a save in feat" l save -qam two
has "the overlay holds feat with its id" "feat $(id_of feat)" "$(lines_of notes)"
ok "a rename" l line rename feat feat2
has "the line on the overlay takes the new name" "feat2 $(id_of feat2)" "$(lines_of notes)"
cd "$P/lines/feat2" || exit 1
eq "the folder still shows its part" n2 "$(cat notes.md)"
echo n3 > notes.md
ok "a save after the rename" l save -qam three
eq "and moves the same line" n3 "$(l overlay show notes head:notes.md)"

cd "$P/lines/main" || exit 1
ok "a delete of feat2" l line delete -D feat2
l gc -q >/dev/null 2>&1
eq "the overlay keeps no line of the deleted line" "main $(id_of main)" "$(lines_of notes)"

# Build output: an ordinary overlay with properties, no cache flag.
mkdir -p target/debug
echo bin > target/debug/x
ok "a save of the build output" l save --overlays -q
TGO=$(l overlay list | awk '/^ *target\./{print $1}' | head -1)
no "the build overlay exists" test -z "$TGO"
eq "no cache flag" "" "$(sqlite3 "$P/meta/db/layr.sqlite" "SELECT value FROM config WHERE key='overlay.$TGO.cache'")"
has "its properties say what it is" "platform=$HERE_PLATFORM trust=protected" "$(l overlay list)"
eq "the build overlay holds main too" "main $(id_of main)" "$(lines_of "$TGO")"
finish
