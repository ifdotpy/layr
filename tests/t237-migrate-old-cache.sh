#!/bin/bash
# ci: macos
# `layr overlay migrate-parts` and a `cache.<folder>` glass of an older layr (docs/overlays.md section 1a): the head
# of each line names its part on the folder glass, and a glass whose states have one platform gets that platform.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
# An older layr: one `cache.target` glass whose saves (`save --glasses`) give the state no part.
l overlay create cache.target --folder target --quiet --history last --merge union-theirs -q
l config overlay.cache.target.cache true >/dev/null
mkdir -p target/debug; echo bin1 > target/debug/x
l save --glasses -q 2>/dev/null
parts() {
    python3 - "$P/meta/db/layr.sqlite" "$1" <<'PY'
import json, sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute("SELECT rec FROM states WHERE id=? OR display=?", (sys.argv[2], sys.argv[2])).fetchone()
print(" ".join(f"{k}={v}" for k, v in sorted(json.loads(row[0]).get("parts", {}).items())) if row else "missing")
PY
}
h=$(l rev-parse head)
eq "the head of the old store names no part" "" "$(parts "$h")"
out=$(l overlay migrate-parts -n)
has "a dry run names the platform it would set" "would set platform $HERE_PLATFORM on cache.target" "$out"
has "and the head" "cache.target=" "$out"
ok "the migration" l overlay migrate-parts -q
has "the head names its part on the folder glass" "cache.target=" "$(parts "$h")"
has "the old glass is the glass of this platform" "platform=$HERE_PLATFORM" "$(l overlay list)"
ok "a new line from the head" l line new -q warm main
eq "it has the build output" bin1 "$(cat "$P/lines/warm/target/debug/x" 2>/dev/null)"
finish
