#!/bin/bash
# ci: macos
# Parts of an overlay are v2 objects (docs/overlays.md section 1a, slice 7): in a project that keeps its states as
# objects, a named save writes the part of a build-output overlay to the overlay's own object store, and a second
# save of the same build adds almost nothing.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l migrate >/dev/null 2>&1
eq "the project keeps its states as objects" "true" "$(l config objects.enabled 2>/dev/null | tr -d '"' | tail -1)"
G=$TG
# part_of <state>: the part of the overlay that a state record names.
part_of() {
    python3 - "$P/meta/db/layr.sqlite" "$1" "$G" <<'PY'
import json, sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute("SELECT rec FROM states WHERE id=? OR display=?", (sys.argv[2], sys.argv[2])).fetchone()
print(json.loads(row[0]).get("parts", {}).get(sys.argv[3], "") if row else "")
PY
}
objects_bytes() { du -sk "$P/meta/overlays/$G/meta/objects" 2>/dev/null | cut -f1; }

mkdir -p target/debug
for i in $(seq 1 50); do head -c 20000 /dev/urandom > "target/debug/unit$i.rlib"; done
echo a >> notes.txt
ok "a named save" l save -qam "first build"
p1=$(part_of "$(l rev-parse head)")
no "the state names a part" test -z "$p1"
ok "the overlay has an object store" test -f "$P/meta/overlays/$G/meta/objects/config.json"
ok "the part is a state of that object store" test -f "$P/meta/overlays/$G/meta/objects/states/$p1"
first=$(objects_bytes)

# The same build again (only the times change): almost no new bytes.
touch target/debug/unit1.rlib
echo b >> notes.txt
ok "a second named save" l save -qam "same build"
p2=$(part_of "$(l rev-parse head)")
second=$(objects_bytes)
if [ -n "$p2" ] && [ "$p2" != "$p1" ]; then
  ok "the new part is in the object store" test -f "$P/meta/overlays/$G/meta/objects/states/$p2"
fi
ok "a part of the same build adds under 100 KB" test $(( second - first )) -lt 100

# The native copy of a build-output part goes once its objects are durable (docs/overlays.md section 1a, slice 7).
cur=${p2:-$p1}
no "the part of the head has no native copy" test -d "$P/meta/overlays/$G/states/$cur"
ok "its objects are in the overlay store" test -f "$P/meta/overlays/$G/meta/objects/states/$cur"

# history=last dropped the first part; gc gives its objects back.
if [ -n "$p2" ] && [ "$p2" != "$p1" ]; then
  l gc -q >/dev/null 2>&1
  no "gc removes the object record of the dropped part" test -f "$P/meta/overlays/$G/meta/objects/states/$p1"
  ok "and keeps the one of the head's part" test -f "$P/meta/overlays/$G/meta/objects/states/$p2"
fi

# A new line from that state shows the build output: the copy comes back from the objects.
l line new other main >/dev/null 2>&1
ok "a new line has the build output" test -f "$P/lines/other/target/debug/unit50.rlib"

# A state that is here only as objects (its native copy retired) still goes to a store remote.
hid=$(sqlite3 "$P/meta/db/layr.sqlite" "select id from states where display='$(l rev-parse head)'")
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
no "the head has no native copy" test -d "$P/states/$hid"
ok "a push" l push cloud -q
ok "the remote has the head that is only objects here" test -d "$T/archive/states/$hid"
finish
