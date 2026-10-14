#!/bin/bash
# Metadata stream cache: exact output, damage recovery, and mutable-tree exclusion.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init perf --from "$T/src" -q
cd "$LAYR_ROOT/projects/perf/lines/main" || exit 1
base=$(l rev-parse HEAD)
echo cache-check >> notes.txt
l commit -qam change
head=$(l rev-parse HEAD)
cache=$LAYR_ROOT/projects/perf/meta/diff-cache
first=$(l diff "$base" "$head")
second=$(l diff "$base" "$head")
eq "cached diff keeps exact output" "$first" "$second"
ok "immutable diff creates a private cache" test -d "$cache"
has "diff contains the changed line" '+cache-check' "$second"
# Damaged entries must cause a fresh kernel stream, never an empty diff.
for f in "$cache"/*; do printf 'damaged\n' > "$f"; done
third=$(l diff "$base" "$head")
eq "damaged cache falls back to a fresh diff" "$first" "$third"
count=$(find "$cache" -type f | wc -l)
echo unsaved >> notes.txt
has "status sees a later write" 'notes.txt' "$(l status --porcelain)"
l status --porcelain >/dev/null
eq "temporary snapshots do not fill the cache" "$count" "$(find "$cache" -type f | wc -l)"
# A stale state UUID path must not select the old cached stream.
l commit -qam later
has "a new state changes the cache identity" '+unsaved' "$(l diff "$base" HEAD)"
finish
