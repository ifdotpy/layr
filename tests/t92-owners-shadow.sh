#!/bin/bash
# Owners in shadow mode (docs/ownership.md, phase 1). After each step the owners table equals a
# replay of the records (fsck), and gc --dry-run prints how its plan differs from the owners. The
# plan still decides what goes, so the difference lines explain each difference, not a failure.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init o --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/o/lines
cd $P/main

# check <step>: fsck finds no problem, and gc --dry-run prints both difference lines.
check() {
    eq "fsck after $1" "0 problem(s)" "$(l fsck | tail -1)"
    local plan
    plan=$(l gc --dry-run)
    has "gc difference (frees) after $1" "owners: plan frees" "$plan"
    has "gc difference (keeps) after $1" "owners: plan keeps" "$plan"
    printf '%s\n' "$plan" | grep -E '^(owners|	)' | sed "s/^/  $1: /"
}

# A table that differs from the records is a problem: fsck counts it, and --repair rewrites it.
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

# A row that the records do not give is found and repaired.
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
