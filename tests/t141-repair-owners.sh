#!/bin/bash
# ci: macos overlayfs
# A repair replay restores the exact owners after ordinary history and ring changes.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
for n in $(seq 1 12); do echo "$n" > notes.txt; l save -q; done
l config undo.ops 3 >/dev/null
l config undo.ops 20 >/dev/null
l gc --repair -q >/dev/null
DB=$LAYR_ROOT/projects/p/meta/db/layr.sqlite
[ -f "$DB" ] || DB=$LAYR_ROOT/projects/p/meta/layr.sqlite
sqlite3 "$DB" 'select state,kind,holder from owners where kind != "pin" order by state,kind,holder' > "$T/before"
sqlite3 "$DB" "delete from owners where rowid=(select rowid from owners limit 1); insert into owners values('01999999-0000-7000-8000-000000000000','tag','stray');"
ok "repair succeeds after missing and extra owners" l gc --repair -q
sqlite3 "$DB" 'select state,kind,holder from owners where kind != "pin" order by state,kind,holder' > "$T/after"
ok "repair restores every owner row" cmp "$T/before" "$T/after"
finish
