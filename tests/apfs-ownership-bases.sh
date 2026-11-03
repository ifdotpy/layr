#!/bin/bash
# APFS: a dead state that is the base of a live thin layer keeps its storage (docs/ownership.md,
# "Storage that a live state needs"). It goes when the layer goes: here the line delete gives the
# live state a complete tree, and the base goes with the delete.
. "$(dirname "$0")/lib.sh"
[ "$(uname -s)" = Darwin ] || { echo "$(basename "$0"): skipped (APFS only)"; exit 0; }

# A state folder is named by the UUID that ids::display maps to the id (as in t90).
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
sample $T/src
l init o --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/o
DB=$P/meta/db/layr.sqlite
cd $P/lines/main || exit 1
l config undo.ops 1 >/dev/null
l config retention.auto 1 >/dev/null
l line new -q w >/dev/null
cd $P/lines/w || exit 1
echo one > auto.txt; a=$(l save | awk '{print $NF}')
echo two > auto.txt; b=$(l save | awk '{print $NF}')
A=$(dashed "$a"); B=$(dashed "$b")
present() { python3 -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); r=c.execute("select present from states where display=?", (sys.argv[2],)).fetchone(); print(r[0] if r else "missing")' "$DB" "$1"; }
on_disk() { [ -e "$P/states/$1" ] || [ -e "$P/deltas/$1" ]; }

# The newest save names the state it changes from (its undo reads it), so that state stays.
eq "the state before the newest save stays present (undo of the save reads it)" 1 "$(present "$a")"
ok "the dead base keeps its storage while the live layer uses it" on_disk "$A"
ok "the live state is a thin layer over the dead base" test -d "$P/deltas/$B"
cd $P/lines/main || exit 1
l line delete -D w >/dev/null
no "the base goes when the live layer goes" on_disk "$A"
eq "the final state keeps its record as present" 1 "$(present "$b")"
eq "fsck is clean" "0 problem(s)" "$(l fsck | tail -1)"
finish
