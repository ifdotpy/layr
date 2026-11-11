#!/bin/bash
# ci: macos
# `clone --line <line>` (docs/cache-overlays.md): a clone for one line downloads only the caches that a new line
# from it takes, the newest cache of the line or of its nearest ancestor, and not the caches of every other line.
# The newest cache of this platform counts: a newer one of another platform stays on the remote. A plain clone
# still takes all usable caches, and a cache that a sweep deleted during a clone only omits.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
echo '/target' >> "$T/src/.gitignore"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
self() { env LAYR_ROOT="$T/$1/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])'; }
for m in x y s; do read id key <<<"$(self $m)"; l remote trust cloud "$id" --public-key "$key" >/dev/null; done
mkdir -p target; echo main > target/a
# The cache of main's head comes from another line, as a check runner saves it (main.checks-<platform>), and a newer
# one of another platform follows (an x86 runner after an arm one): this platform's cache still comes.
l line new runner main >/dev/null 2>&1
(cd "$P/runner" && mkdir -p target && echo main > target/a && l save --caches -q \
    && echo plan9 > target/a && LAYR_TEST_PLATFORM=plan9-mips l save --caches -q)
l line new other main >/dev/null 2>&1
(cd "$P/other" && echo other > o.txt && l add o.txt >/dev/null && l save -qm other && echo other > target/a && head -c 300000 /dev/urandom > target/big && l save --caches -q)
l push cloud -q >/dev/null 2>&1

xl() { env LAYR_ROOT="$T/x/store" "$LAYR" "$@"; }
out=$(xl clone "file://$T/archive" p --line main 2>&1)
has "the clone names the caches it leaves on the remote" "not for --line" "$out"
(cd "$T/x/store/projects/p/lines/main" && xl line new w main >/dev/null 2>&1)
eq "a new line of main starts warm" main "$(cat "$T/x/store/projects/p/lines/w/target/a" 2>&1)"
(cd "$T/x/store/projects/p/lines/main" && xl line new w2 other >/dev/null 2>&1)
no "the other line's cache did not come" test -e "$T/x/store/projects/p/lines/w2/target/big"
has "fsck of the clone is clean" "0 unreachable state(s)" "$(cd "$T/x/store/projects/p/lines/main" && xl fsck --caches 2>&1)"
ok "the clone still pushes" sh -c "cd '$T/x/store/projects/p/lines/w' && echo w > target/w && env LAYR_ROOT='$T/x/store' '$LAYR' save --caches -q && env LAYR_ROOT='$T/x/store' '$LAYR' push origin -q"
no "an unknown line is refused" env LAYR_ROOT="$T/y2/store" "$LAYR" clone "file://$T/archive" p --line nosuch

sl() { env LAYR_ROOT="$T/s/store" "$LAYR" "$@"; }
sl clone "file://$T/archive" p --heads --only-line main --line main >/dev/null 2>&1
eq "a selected clone with caches succeeds" 0 "$?"
no "a selected clone leaves another line unmade" test -e "$T/s/store/projects/p/lines/other"
(cd "$T/s/store/projects/p/lines/main" && sl line new warm main >/dev/null 2>&1)
eq "a new line from the selected clone starts warm" main "$(cat "$T/s/store/projects/p/lines/warm/target/a" 2>&1)"

yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
yl clone "file://$T/archive" p >/dev/null 2>&1
(cd "$T/y/store/projects/p/lines/main" && yl line new y2 other >/dev/null 2>&1)
ok "a plain clone takes the caches of every line" test -e "$T/y/store/projects/p/lines/y2/target/big"

# A push sweep that deletes a cache state while a clone runs: the clone omits that cache and goes on.
find "$T/archive/overlays" \( -name '*.data.age' -o -name '*.pack.age' -o -name '*.index.age' \) -delete
zl() { env LAYR_ROOT="$T/z/store" "$LAYR" "$@"; }
read zid zkey <<<"$(self z)"; (cd "$P/main" && l remote trust cloud "$zid" --public-key "$zkey" >/dev/null && l save --caches -q && l push cloud -q >/dev/null 2>&1)
out=$(zl clone "file://$T/archive" p 2>&1); code=$?
eq "a clone whose caches went away on the remote works" 0 "$code"
has "and says which cache it did not restore" "not restored" "$out"
finish
