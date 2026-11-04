#!/bin/bash
# ci: macos overlayfs
# Replay a deleted line and a resized undo ring through the actual repair command.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main"
l line new -q work >/dev/null
cd "$P/work"
for n in 1 2 3; do echo "$n" > notes.txt; l save -q; done
cd "$P/main"
l line delete -D work >/dev/null
l config undo.ops 2 >/dev/null
l config undo.ops 20 >/dev/null
l gc --repair -q >/dev/null
DB=$LAYR_ROOT/projects/p/meta/db/layr.sqlite
[ -f "$DB" ] || DB=$LAYR_ROOT/projects/p/meta/layr.sqlite
sqlite3 "$DB" 'select state,kind,holder from owners where kind != "pin" order by state,kind,holder' > "$T/before"
ok "a second repair succeeds" l gc --repair -q
sqlite3 "$DB" 'select state,kind,holder from owners where kind != "pin" order by state,kind,holder' > "$T/after"
ok "deleted-line owner rows remain identical" cmp "$T/before" "$T/after"
ok "gc sees no owner differences" l gc -q
finish
