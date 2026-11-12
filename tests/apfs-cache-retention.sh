#!/bin/bash
# Caches live in cache overlays only: save --overlays makes no cache snapshot of a state, and the old
# snapshots of states that older layr made (`states/<state>@<path>`) go to the trash at the next
# save --overlays or gc, while the states stay. Nothing is left in the trash or in meta/work.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init ret --from "$T/src" -q >/dev/null
S=$LAYR_ROOT/projects/ret
P=$S/lines
snaps() { ls "$S/states" | grep -c '@'; }
cd "$P/main" || exit 1
l line new -q topic
cd "$P/topic" || exit 1
printf 'target/\n' >> .gitignore
printf 'target regenerable\n' > .layrattributes
l add -A && l save -qm attributes

# 1. save --overlays keeps the build output in the cache overlay, not as a snapshot of the state.
mkdir -p target && printf 'out 1\n' > target/out
out=$(l save --overlays)
has "the cache goes to its overlay" "target: overlay" "$out"
eq "no cache snapshot of a state" "0" "$(snaps)"
eq "save --overlays leaves no copies" "" "$(ls -A "$S/meta/work" 2>/dev/null)"
l line new -q warm topic
eq "a new line takes the cache from the overlay" "out 1" "$(cat "$P/warm/target/out" 2>/dev/null)"

# 2. An old snapshot of a state goes at the next save --overlays; the state stays.
head=$(l rev-parse head)
mkdir -p "$S/states/$head@target" && printf 'old\n' > "$S/states/$head@target/out"
printf 'out 2\n' > target/out
l save -q --overlays
eq "save --overlays removes an old snapshot" "0" "$(snaps)"
ok "the state stays" l show "$head:.layrattributes"

# 3. gc lists and removes one too.
mkdir -p "$S/states/$head@target"
has "gc --dry-run lists the old snapshots" "would delete 1 cache(s)" "$(l gc --repair --dry-run)"
eq "a dry run removes nothing" "1" "$(snaps)"
ok "gc" l gc --repair -q
eq "gc removes it" "0" "$(snaps)"
eq "gc leaves no trash" "" "$(ls -A "$S/meta/trash" 2>/dev/null)"
ok "states remain valid" l fsck
finish
