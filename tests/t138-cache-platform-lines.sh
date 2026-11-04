#!/bin/bash
# ci: macos overlayfs
# A line has one cache overlay line per platform (`line-<line id>.<os>-<arch>`, docs/cache-overlays.md,
# section 6): a `save --caches` on another platform does not replace this platform's cache, and a new
# line takes the cache of its own platform. An overlay line of an older layr (`line-<line id>`) gets
# the platform name of its head at the first write. Test hook: LAYR_TEST_PLATFORM acts as another platform.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
rm -rf target; mkdir target; echo built-here > target/out
l save --caches -q
echo built-there > target/out
LAYR_TEST_PLATFORM=plan9-mips l save --caches -q
echo built-here-2 > target/out
l save --caches -q

os=linux; [ "$(uname -s)" = Darwin ] && os=macos
l line new here main >/dev/null 2>&1
eq "a new line takes the cache of this platform" built-here-2 "$(cat $P/here/target/out 2>/dev/null)"
LAYR_TEST_PLATFORM=plan9-mips l line new there main >/dev/null 2>&1
eq "a new line on the other platform takes that platform's cache" built-there "$(cat $P/there/target/out 2>/dev/null)"

# line delete removes the overlay lines of every platform of the line.
cd $P/here || exit 1
here_id=$(l line list --json 2>/dev/null | python3 -c 'import json,sys; print(next(l["id"] for l in json.load(sys.stdin) if l["name"]=="here"))' 2>/dev/null)
echo here-own > target/out
l save --caches -q
echo there-own > target/out
LAYR_TEST_PLATFORM=plan9-mips l save --caches -q
cd $P/main || exit 1
odb=$LAYR_ROOT/projects/p/meta/overlays/cache.target/meta/db/layr.sqlite
names() { python3 -c 'import sqlite3,sys; print(" ".join(sorted(r[0] for r in sqlite3.connect(sys.argv[1]).execute("select name from lines where coalesce(deleted, 0) = 0"))))' "$odb"; }
eq "the line has an overlay line per platform" "line-$here_id.$os line-$here_id.plan9-mips" "$(names | tr ' ' '\n' | grep "$here_id" | sed "s/\.$os-[a-z0-9_]*/.$os/" | tr '\n' ' ' | sed 's/ $//')"
l line delete -D here >/dev/null
no "line delete removes the overlay lines of both platforms" grep -q "$here_id" <<<"$(names)"
finish
