#!/bin/bash
# Owners (docs/ownership.md): after each step the owners table equals a replay of the records
# (fsck and gc check agree), no present state lacks an owner (a leak), and undo of the line
# delete works. The step list is the one of the shadow phase (#115).
. "$(dirname "$0")/lib.sh"

sample $T/src
l init o --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/o/lines
cd $P/main

# check <step>: fsck finds no problem, and gc check exits 0 (no leak, no difference).
check() {
    eq "fsck after $1" "0 problem(s)" "$(l fsck | tail -1)"
    eq "gc check after $1" 0 "$(l gc >/dev/null 2>&1; echo $?)"
}

# The owners table file: the database of the project.
db=$LAYR_ROOT/projects/o/meta/db/layr.sqlite
[ -f "$db" ] || db=$LAYR_ROOT/projects/o/meta/layr.sqlite

# Line create.
l line new -q w
check "line create"

# Saves on main.
echo a >> notes.txt; l save -qam c1
echo b >> notes.txt; l save -qam c2
check "two saves"

# Automatic states past the owners ring (retention.auto, default 10): 12 saves on w make 12.
cd $P/w
for i in $(seq 1 12); do echo "$i" > auto.txt; l save -q; done
n_auto=$(l log --auto --format=%h | wc -l | tr -d ' ')
ok "automatic states past the ring" test "$n_auto" -gt 10
check "automatic states past the ring"
cd $P/main

# Refs: set, move, delete.
l ref set env/prod head~1
check "ref set"
l ref set env/prod head --expect "$(l rev-parse head~1)"
check "ref move"
l ref delete env/prod
check "ref delete"

# Tag: create and delete.
l tag v1
check "tag"
l tag -d v1
check "tag delete"

# Park and pop: unfinished work leaves the working folder and comes back.
echo parked >> notes.txt
l park >/dev/null
check "park"
l park pop >/dev/null
check "park pop"

# A row that the records do not give is a difference: fsck counts it, and --repair rewrites it.
cd $P/main
sqlite3 "$db" "INSERT INTO owners(state, kind, holder) VALUES('01999999-0000-7000-8000-000000000000', 'tag', 'stray')"
eq "fsck finds a stray owner" "1 problem(s)" "$(l fsck | tail -1)"
l fsck --repair >/dev/null
eq "fsck --repair rewrites the owners" "0 problem(s)" "$(l fsck | tail -1)"

# Line delete, then undo of the delete.
l line delete -D w
check "line delete"
l undo >/dev/null
check "undo of line delete"

finish
