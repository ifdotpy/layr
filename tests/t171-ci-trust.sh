#!/bin/bash
# ci: macos overlayfs
# Limited machine trust (docs/ci-trust.md). A cache writer job saves caches on a save-back line and they reach the
# other machines; its source states, other lines and trusts count nowhere. A reader makes no record that others
# take. A record signed after the trust ended has no effect. `remote untrust` makes a new key epoch without the
# machine.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
mkdir -p target && echo a > target/a && l save --glasses -q
ov=$TG
plat=$(uname -s | tr A-Z a-z)-$(uname -m | sed 's/arm64/aarch64/')
self() { env LAYR_ROOT="$T/$1/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])'; }
read w_id w_key <<<"$(self w)"
read r_id r_key <<<"$(self r)"
read e_id e_key <<<"$(self e)"
out=$(l remote trust cloud "$w_id" --public-key "$w_key" --expires 1h --role writer --overlays "$ov" --json)
has "a trust records its role" '"role":"writer"' "$out"
has "and its end" '"expires_at":' "$out"
no "an end in the past is refused" l remote trust cloud "$r_id" --public-key "$r_key" --expires 2000-01-01T00:00:00Z
no "an unknown overlay is refused" l remote trust cloud "$r_id" --public-key "$r_key" --expires 1h --overlays nosuch
l remote trust cloud "$r_id" --public-key "$r_key" --expires 1h >/dev/null
l remote trust cloud "$e_id" --public-key "$e_key" --expires 6s --role writer >/dev/null
trusted_at=$(date +%s)
l push cloud -q >/dev/null 2>&1

# A merge machine: a full machine narrowed to the merge role. Its release ref counts; another ref and a new line
# do not. A broker cannot hand out the role.
read m_id m_key <<<"$(self m)"
l remote trust cloud "$m_id" --public-key "$m_key" >/dev/null
out=$(l remote trust cloud "$m_id" --public-key "$m_key" --role merge --json)
has "a trust narrows a full machine to the merge role" '"role":"merge"' "$out"
l push cloud -q >/dev/null 2>&1
ml() { env LAYR_ROOT="$T/m/store" "$LAYR" "$@"; }
(cd "$T" && ml clone "file://$T/archive" p >/dev/null 2>$T/m-clone.err) || cat $T/m-clone.err
M=$T/m/store/projects/p/lines
(cd "$M/main" && ml ref set release/v1 main >/dev/null && ml ref set env/prod main >/dev/null \
  && ml line new mx main >/dev/null 2>&1 && ml push origin -q >/dev/null 2>&1)
l fetch cloud -q >/dev/null 2>&1
refs=$(l ref list 2>&1)
has "the merge machine's release ref arrives" "release/v1" "$refs"
no "its other ref does not" grep -q "env/prod" <<<"$refs"
no "nor its new line" grep -qx '  mx' <<<"$(l line list 2>&1)"
has "a machine with limits cannot become a merge machine" "has limits already" \
  "$(l remote trust cloud "$r_id" --public-key "$r_key" --role merge 2>&1)"
has "a broker cannot delegate the merge role" "use reader or writer" \
  "$(l access grant "machine:$r_id" trust --max-hours 1 --role merge 2>&1)"

# The writer: a cache on its save-back line counts; a source state, another line and a trust do not.
wl() { env LAYR_ROOT="$T/w/store" "$LAYR" "$@"; }
(cd "$T" && wl clone "file://$T/archive" p --line main >/dev/null 2>&1)
W=$T/w/store/projects/p/lines
(cd "$W/main" && wl line new "main.checks-$plat" main >/dev/null 2>&1)
(cd "$W/main.checks-$plat" && echo w > target/w && wl save --glasses -q)
(cd "$W/main" && wl line new wx main >/dev/null 2>&1 && cd "$W/wx" && echo w > w.txt && wl add w.txt >/dev/null && wl save -qm "a source state of the writer")
(cd "$W/main" && wl push origin -q >/dev/null 2>&1)
l fetch cloud -q >/dev/null 2>&1
lines=$(l line list 2>&1)
has "the save-back line of the writer arrives" "main.checks-$plat" "$lines"
no "its other line does not" grep -qx '  wx' <<<"$lines"
eq "its source state is not taken" "" "$(l log --all --json 2>/dev/null | grep 'a source state of the writer')"
(cd "$LAYR_ROOT/projects/p/lines/main" && l line new back "main.checks-$plat" >/dev/null 2>&1)
ok "its cache comes with a line from the save-back line" test -f "$LAYR_ROOT/projects/p/lines/back/target/w"

# The reader: its line counts nowhere.
rl() { env LAYR_ROOT="$T/r/store" "$LAYR" "$@"; }
(cd "$T" && rl clone "file://$T/archive" p >/dev/null 2>&1)
(cd "$T/r/store/projects/p/lines/main" && rl line new "main.checks-reader" main >/dev/null 2>&1 && rl push origin -q >/dev/null 2>&1)
l fetch cloud -q >/dev/null 2>&1
no "a reader's line does not arrive" grep -q 'main.checks-reader' <<<"$(l line list 2>&1)"

# After the end: a record of the machine has no effect.
el() { env LAYR_ROOT="$T/e/store" "$LAYR" "$@"; }
(cd "$T" && el clone "file://$T/archive" p >/dev/null 2>&1)
while [ $(($(date +%s) - trusted_at)) -lt 8 ]; do sleep 1; done
(cd "$T/e/store/projects/p/lines/main" && el line new "late.checks-$plat" main >/dev/null 2>&1 && el push origin -q >/dev/null 2>&1)
l fetch cloud -q >/dev/null 2>&1
no "a line made after the trust ended does not arrive" grep -q "late.checks-$plat" <<<"$(l line list 2>&1)"
# The slots of the newest project key epoch.
epoch_slots() { ls -t "$T/archive/project/epochs" | head -1 | xargs -I{} ls "$T/archive/project/epochs/{}/slots"; }
has "a rotation works" '"rotated":true' "$(l remote rotate cloud --json 2>&1)"
no "it gives no slot to the machine whose trust ended" grep -q "machine-$e_id" <<<"$(epoch_slots)"
has "and keeps the live ones" "machine-$w_id" "$(epoch_slots)"

# A full rebuild of the index gives the same result.
l reindex >/dev/null 2>&1
no "after a rebuild the writer's line is still absent" grep -qx '  wx' <<<"$(l line list 2>&1)"

# Untrust: a new key epoch without the machine.
out=$(l remote untrust cloud "$w_id" --json 2>&1)
has "untrust makes a new epoch" '"rotated":true' "$out"
no "the untrusted machine has no slot" grep -q "machine-$w_id" <<<"$(epoch_slots)"
has "a trusted machine keeps its slot" "machine-$r_id" "$(epoch_slots)"
no "a second untrust of an unknown machine fails" l remote untrust cloud 01a1aaaa-0000-7000-8000-000000000000

garbage_check
finish
