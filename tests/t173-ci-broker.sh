#!/bin/bash
# ci: macos overlayfs
# Broker delegation (docs/ci-trust.md). The owner lets a broker machine trust CI jobs within limits. The broker
# trusts a job for an hour, the job's cache reaches the owner, and a trust outside the delegation is refused. After
# the revoke the broker trusts nobody.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
mkdir -p target && echo a > target/a && l save --caches -q
ov=$(l overlay list --json 2>/dev/null | python3 -c 'import json,sys; print([o["name"] for o in json.load(sys.stdin) if o["name"].startswith("cache.")][0])')
plat=$(uname -s | tr A-Z a-z)-$(uname -m | sed 's/arm64/aarch64/')
self() { env LAYR_ROOT="$T/$1/store" LAYR_MACHINE_NAME="$2" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"], d["name"])'; }
read b_id b_key b_name <<<"$(self b runner)"
read j_id j_key j_name <<<"$(self j ci-job-1)"
eq "a new machine takes its name from LAYR_MACHINE_NAME" ci-job-1 "$j_name"
l remote trust cloud "$b_id" --public-key "$b_key" >/dev/null
out=$(l access grant "machine:$b_id" trust --max-hours 2 --role writer --overlays "$ov" --name-prefix ci- --json)
has "the owner delegates to the broker" '"max_hours":2' "$out"
no "a delegation needs a trusted machine" l access grant machine:01a1aaaa-0000-7000-8000-000000000000 trust --max-hours 2
mid=$(l remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
l overlay managers "$ov" "machine:$mid" "machine:$b_id" >/dev/null 2>&1
l push cloud -q --caches >/dev/null 2>&1

bl() { env LAYR_ROOT="$T/b/store" "$LAYR" "$@"; }
(cd "$T" && bl clone "file://$T/archive" p >/dev/null 2>&1)
cd "$T/b/store/projects/p/lines/main" || exit 1
no "the broker makes no full trust" bl remote trust origin "$j_id" --public-key "$j_key"
no "nor one longer than its delegation" bl remote trust origin "$j_id" --public-key "$j_key" --name "$j_name" --expires 3h --role writer --overlays "$ov"
no "nor one for a name without the prefix" bl remote trust origin "$j_id" --public-key "$j_key" --name job-1 --expires 1h --role writer --overlays "$ov"
out=$(bl remote trust origin "$j_id" --public-key "$j_key" --name "$j_name" --expires 1h --role writer --overlays "$ov" --json)
has "the broker trusts the job inside its delegation" '"role":"writer"' "$out"
bl push origin -q --caches >/dev/null 2>&1

jl() { env LAYR_ROOT="$T/j/store" "$LAYR" "$@"; }
(cd "$T" && jl clone "file://$T/archive" p --caches-of main >/dev/null 2>&1)
J=$T/j/store/projects/p/lines
(cd "$J/main" && jl line new w main >/dev/null 2>&1)
ok "the job reads the cache of main" test -f "$J/w/target/a"
(cd "$J/main" && jl line new "main.checks-$plat" main >/dev/null 2>&1)
(cd "$J/main.checks-$plat" && echo j > target/j && jl save --caches -q && jl push origin -q --caches >/dev/null 2>&1)
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l fetch cloud -q >/dev/null 2>&1
has "the job's save-back line reaches the owner" "main.checks-$plat" "$(l line list 2>&1)"

# The revoke: the broker trusts nobody from then on.
l access revoke "machine:$b_id" trust >/dev/null
l push cloud -q >/dev/null 2>&1
read k_id k_key k_name <<<"$(self k ci-job-2)"
(cd "$T/b/store/projects/p/lines/main" && bl fetch origin -q >/dev/null 2>&1)
no "after the revoke the broker trusts no job" bl remote trust origin "$k_id" --public-key "$k_key" --name "$k_name" --expires 1h --role writer --overlays "$ov"

garbage_check
finish
