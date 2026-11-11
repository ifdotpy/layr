#!/bin/bash
# ci: macos overlayfs
# Build output of each platform is its own glass (docs/overlays.md section 1a, slice 3): a `save` on
# another platform makes the glass of that platform (`target.<os>-<arch>`) and does not replace this
# platform's part, and a new line takes the part of its own platform. The overlay lines of a glass
# have no platform key (`line-<line id>`). An older `cache.<folder>` glass keeps one overlay line per
# platform (t61). Test hook: LAYR_TEST_PLATFORM acts as another platform.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
rm -rf target; mkdir target; echo built-here > target/out
l save --glasses -q
echo built-there > target/out
LAYR_TEST_PLATFORM=plan9-mips l save --glasses -q
echo built-here-2 > target/out
l save --glasses -q

l line new here main >/dev/null 2>&1
eq "a new line takes the cache of this platform" built-here-2 "$(cat $P/here/target/out 2>/dev/null)"
LAYR_TEST_PLATFORM=plan9-mips l line new there main >/dev/null 2>&1
eq "a new line on the other platform takes that platform's cache" built-there "$(cat $P/there/target/out 2>/dev/null)"

# line delete removes the overlay lines of every platform of the line.
cd $P/here || exit 1
here_id=$(l line list --json 2>/dev/null | python3 -c 'import json,sys; print(next(l["id"] for l in json.load(sys.stdin) if l["name"]=="here"))' 2>/dev/null)
echo here-own > target/out
l save --glasses -q
echo there-own > target/out
LAYR_TEST_PLATFORM=plan9-mips l save --glasses -q
cd $P/main || exit 1
names() { python3 -c 'import sqlite3,sys; print(" ".join(sorted(r[0] for r in sqlite3.connect(sys.argv[1]).execute("select name from lines where coalesce(deleted, 0) = 0"))))' "$LAYR_ROOT/projects/p/meta/overlays/$1/meta/db/layr.sqlite"; }
has "each platform has its glass" "$(glass target) $(LAYR_TEST_PLATFORM=plan9-mips glass target)" "$(l overlay list | awk '{print $1}' | sort | paste -sd' ' -)"
eq "the glass of this platform has the line without a platform key" "line-$here_id" "$(names "$TG" | tr ' ' '\n' | grep "$here_id")"
eq "and so has the glass of the other platform" "line-$here_id" "$(names target.plan9-mips | tr ' ' '\n' | grep "$here_id")"
l line delete -D here >/dev/null
no "line delete removes the overlay lines of both glasses" grep -q "$here_id" <<<"$(names "$TG") $(names target.plan9-mips)"
finish
