#!/bin/bash
# Cache retention without a daemon: save --caches and line delete keep only the newest
# retention.caches caches (plus the caches of line heads) and leave no trash; gc --dry-run
# lists the caches it would delete. save --caches copies the caches before the store lock and
# under the lock only renames those copies into place.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init ret --from "$T/src" -q >/dev/null
S=$LAYR_ROOT/projects/ret
P=$S/lines
caches() { ls "$S/states" | grep -c '@target'; }
build() { # build <line> <n>: a commit and its build output, saved as caches
    cd "$P/$1" || exit 1
    printf '%s\n' "$2" > "file$2.txt"
    l add "file$2.txt" && l save -qm "c$2"
    mkdir -p target && printf 'out %s\n' "$2" > target/out
    l save -q --caches
}
cd "$P/main" || exit 1
l line new -q topic
cd "$P/topic" || exit 1
printf 'target/\n' >> .gitignore
printf 'target regenerable\n' > .layrattributes
l add -A && l save -qm attributes
l config retention.caches 1

# 1. Each save --caches keeps only the newest cache of the line.
build topic 1
build topic 2
build topic 3
eq "save --caches keeps one cache" "1" "$(caches)"
eq "save --caches leaves no trash" "" "$(ls -A "$S/meta/trash" 2>/dev/null)"
eq "the kept cache is the newest build" "out 3" "$(cat "$S/states/$(ls "$S/states" | grep '@target')/out" 2>/dev/null)"

# 1b. The copies are made before the lock: under it the save takes them and leaves none.
printf '4\n' > target/out
has "save --caches takes the copies made before the lock" "APFS take prepared copy" "$(LAYR_TRACE=1 l save -q --caches 2>&1)"
printf '5\n' > target/out
no "save --caches copies no cache under the lock" grep -q 'APFS clone .*/states/' <<< "$(LAYR_TRACE=1 l save -q --caches 2>&1)"
eq "the taken copy is the current build" "1" "$(grep -lx 5 "$S"/states/*@target/out 2>/dev/null | wc -l | tr -d ' ')"
eq "save --caches leaves no copies" "" "$(ls -A "$S/meta/work" 2>/dev/null)"
eq "a sealed cache moves to the trash and is deleted" "" "$(ls -A "$S/meta/trash" 2>/dev/null)"

# 2. gc --dry-run lists the caches that a lower limit lets go; gc deletes them.
l config retention.caches 10
build topic 4
build topic 5
eq "a higher limit keeps more caches" "3" "$(caches)"
l config retention.caches 1
has "gc --dry-run lists the caches" "would delete 2 cache(s)" "$(l gc --dry-run)"
eq "a dry run deletes no cache" "3" "$(caches)"
ok "gc" l gc -q
eq "gc keeps one cache" "1" "$(caches)"

# 3. line delete drops the caches that only the deleted line's head kept.
l config retention.caches 0
eq "the head of a line keeps its cache" "1" "$(caches)"
cd "$P/main" || exit 1
ok "delete the line" l line delete -D -q topic
eq "line delete drops the cache of its head" "0" "$(caches)"
eq "line delete leaves no trash" "" "$(ls -A "$S/meta/trash" 2>/dev/null)"
no "the line folder is gone" test -e "$P/topic"
ok "states remain valid" l fsck
finish
