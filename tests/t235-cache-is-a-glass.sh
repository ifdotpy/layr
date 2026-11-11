#!/bin/bash
# ci: macos
# A cache is an ordinary glass (docs/overlays.md section 1a, slices 3 and 6): a `regenerable`
# folder becomes the glass of this platform (`target.<platform>`, no `cache.` prefix) at the first
# named save, `save -m` records its part in the state, a new line takes that part, a glass of
# another platform is not shown and not fetched, `trust` is a glass property, and the old cache
# flags print their replacement and keep working.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true

HERE=$HERE_PLATFORM
G=$TG

# parts <state>: the parts of a state record as `glass=part` pairs (full ids).
parts() {
    python3 - "$P/meta/db/layr.sqlite" "$1" <<'PY'
import json, sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute("SELECT rec FROM states WHERE id=? OR display=?", (sys.argv[2], sys.argv[2])).fetchone()
print(" ".join(f"{k}={v}" for k, v in sorted(json.loads(row[0]).get("parts", {}).items())) if row else "missing")
PY
}
# states <glass>: the number of states in the glass store.
states() { ls "$P/meta/overlays/$1/states" | wc -l | tr -d ' '; }

# 1. A regenerable folder becomes the glass of this platform at the first named save, and the
#    state names its part.
rm -rf target; mkdir -p target/debug; echo bin1 > target/debug/x
echo a >> notes.txt
ok "a named save" l save -qam "first"
list=$(l overlay list)
has "the folder is the glass of this platform" "$G" "$list"
eq "no cache. glass" "" "$(echo "$list" | grep -o 'cache\.target')"
has "the glass has the build-output properties" "folder=target history=last quiet merge=union-theirs platform=$HERE trust=protected" "$list"
h1=$(l rev-parse head)
p1=$(parts "$h1")
has "the state names its part on the glass" "$G=" "$p1"
eq "status does not show the folder" "" "$(l status --short)"

# 2. An unchanged folder keeps its part: no new copy.
n=$(states "$G")
echo b >> notes.txt
ok "a second named save" l save -qam "second"
eq "no new part for an unchanged folder" "$n" "$(states "$G")"
eq "the new state names the same part" "$p1" "$(parts "$(l rev-parse head)")"

# 3. A changed folder gets a new part with the next named save.
echo bin2 > target/debug/x
echo c >> notes.txt
ok "a third named save" l save -qam "third"
h3=$(l rev-parse head)
p3=$(parts "$h3")
has "the state names a part on the glass" "$G=" "$p3"
no "a new part for a changed folder" test "$p3" = "$p1"

# 4. A new line takes the part: from the line, and from an older state.
l line new warm main >/dev/null
eq "a new line from the line has the part" bin2 "$(cat "$P/lines/warm/target/debug/x")"
l line new old "$h3" >/dev/null
eq "a new line from a state has the part that the state names" bin2 "$(cat "$P/lines/old/target/debug/x" 2>/dev/null)"

# 5. trust is a glass property; --platform sets the platform of a glass.
ok "trust readers" l overlay set "$G" --trust readers
has "overlay list shows the trust" "trust=readers" "$(l overlay list)"
ok "trust protected" l overlay set "$G" --trust protected
no "an unknown trust value is refused" l overlay set "$G" --trust everyone
no "an invalid platform key is refused" l overlay create bad --mode follow --platform 'Bad Key'

# 6. A glass of another platform: no folder of this platform, not in the default view, not fetched.
ok "a folder glass of another platform" l overlay create target.other-arch --folder target --platform other-arch --quiet --history last -q
no "a second glass of this platform for the folder is refused" l overlay create target.again --folder target --platform "$HERE" -q
ok "a file glass of another platform" l overlay create far --mode follow --platform other-arch -q
ok "a file glass of this platform" l overlay create near --mode follow --platform "$HERE" -q
l line new fresh main >/dev/null
cd "$P/lines/fresh" || exit 1
glasses=$(l overlay use | awk '{print $1}' | paste -sd' ' -)
has "the default view shows the glass of this platform" "near" "$glasses"
eq "the default view leaves out the glass of another platform" "" "$(echo "$glasses" | grep -ow far)"
eq "the folder keeps the part of this platform" bin2 "$(cat target/debug/x)"
cd "$P/lines/main" || exit 1
mkdir -p "$T/remote"
ok "a store remote" l remote add r "file://$T/remote"
push=$(l push r 2>&1)
has "push leaves out the glass of another platform" "far: a glass of platform other-arch" "$push"
has "and the folder glass of another platform" "target.other-arch: a glass of platform other-arch" "$push"
fetch=$(l fetch r 2>&1)
eq "fetch takes no glass of another platform" "" "$(echo "$fetch" | grep -E 'far|other-arch' | grep -v 'a glass of platform')"

# 7. No flag names caches (owner decision 2026-11-11): `save --glasses` saves the parts of the folder
#    glasses, and the old spellings fail with exit status 2 and name the new ones.
echo bin3 > target/debug/x
out=$(l save --glasses 2>&1)
has "save --glasses saves the glass" "target: overlay $G" "$out"
out=$(l save --caches 2>&1); code=$?
eq "save --caches fails with status 2" 2 "$code"
has "and names --glasses" "layr save --glasses" "$out"
eq "fsck --caches fails with status 2" 2 "$(l fsck --caches >/dev/null 2>&1; echo $?)"
ok "fsck --glasses" l fsck --glasses
# The cache flags of transfers are gone (owner decision 2026-11-11): exit status 2 and the replacement.
out=$(l fetch r --no-caches 2>&1); code=$?
eq "fetch --no-caches fails with status 2" 2 "$code"
has "and names --overlay" "--overlay base" "$out"
out=$(l fetch r --caches-of main -q 2>&1); code=$?
eq "fetch --caches-of fails with status 2" 2 "$code"
has "and names --line" "--line <line>" "$out"
eq "push --caches fails with status 2" 2 "$(l push r --caches -q >/dev/null 2>&1; echo $?)"
cd "$P/lines/warm" || exit 1
eq "merge --caches-only fails with status 2" 2 "$(l merge main --caches-only >/dev/null 2>&1; echo $?)"

finish
