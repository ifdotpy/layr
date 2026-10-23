#!/bin/bash
# Addressable overlay stacks, exact materialization, reader rights and view attestations.
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src"
echo shared > "$T/src/code"
l init viewtest --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/viewtest/lines
cd "$P/main"
SHARED=$(l rev-parse HEAD)
EMPTY=$(l view --json)
E=$(printf '%s' "$EMPTY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
eq "view ids use full SHA-256" 64 "${#E}"
ok "empty view has canonical identity" python3 -c 'import hashlib,json,sys; d=json.loads(sys.argv[1]); v=d["view"]; c={"format":1,"shared":v["shared"],"layers":v["layers"]}; assert hashlib.sha256(json.dumps(c,separators=(",",":")).encode()).hexdigest()==d["view_id"] and not v["layers"]' "$EMPTY"
has "plain view prints its id" "$E" "$(l view)"
no "computed views do not resolve before save" l rev-parse "views/$E"
l view save --json >/dev/null
BEFORE=$(l op log --all --json -n 10000 | python3 -c 'import json,sys; print(sum(json.loads(x)["op"]=="view.save" for x in sys.stdin))')
l view save main --json >/dev/null
AFTER=$(l op log --all --json -n 10000 | python3 -c 'import json,sys; print(sum(json.loads(x)["op"]=="view.save" for x in sys.stdin))')
eq "saving a view twice writes one record" "$BEFORE" "$AFTER"
eq "a view revision resolves to its shared state" "$SHARED" "$(l rev-parse "views/$E")"
eq "show of a view reads the shared state" shared "$(l show "views/$E:code")"
no "a malformed view id fails" l rev-parse views/short
no "an unknown view id fails" l rev-parse "views/$(printf '%064d' 0)"

# A saved stack pins exact overlay states, including an old shared-mode state.
echo private-alpha > .private.env
l overlay create private --mode shared -q
l overlay move private .private.env >/dev/null
l commit -qm add-private-file
V=$(l view save --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
no "a stack has a different identity from its shared state" test "$V" = "$E"
STATE_VIEW=$(l view "$SHARED" --json)
eq "a state revision has no overlay layers" 0 "$(printf '%s' "$STATE_VIEW" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["view"]["layers"]))')"
echo uncommitted >> .private.env
eq "working edits do not change a view id" "$V" "$(l view --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')"
l restore .private.env
echo private-beta > .private.env
l commit -qam update-private-file
V2=$(l view save --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
no "a changed overlay state changes the view id" test "$V" = "$V2"
ok "branch accepts a saved view" l branch -q exact "views/$V"
eq "the new line has the exact old private content" private-alpha "$(cat "$P/exact/.private.env")"
eq "the new line keeps the shared head" "$SHARED" "$(l --project viewtest --line exact rev-parse HEAD)"
eq "the new line has the exact saved stack" "$V" "$(l --project viewtest --line exact view --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')"
eq "the existing shared overlay line stays unchanged" private-beta "$(l overlay show private user-0:.private.env)"
cd "$P/exact"; echo edit >> .private.env
has "a stale stack keeps the existing commit guard" 'changed in another line' "$(l commit -qam stale 2>&1)"
l restore .private.env
cd "$P/main"
ok "batch branch accepts a view" l branch batch "views/$V" --count 2 -q
eq "a batch line preserves the exact stack" "$V" "$(l --project viewtest --line batch-2 view --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')"

# Full-view try records evidence against the view, not only the shared state.
ok "try materializes the saved overlay state" l try -q "views/$V" -- sh -c 'test "$(cat .private.env)" = private-alpha && test "$(cat code)" = shared'
ok "try provides the exact view id" l try -q "views/$V" -- sh -c 'test "$LAYR_TRY_VIEW" = "$1"' sh "$V"
ok "try of a line uses its stack" l try -q main -- sh -c 'test "$(cat .private.env)" = private-beta'
JSON=$(l attestations "views/$V" --kind check --json)
ok "checks have view subjects" python3 -c 'import json,sys; rows=json.loads(sys.argv[1]); assert len(rows)==2 and all(x["subject"]=={"kind":"view","id":sys.argv[2]} for x in rows)' "$JSON" "$V"
eq "a line check names its current view" 1 "$(l attestations "views/$V2" --kind check --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ok "a state-only try has no private content" l try -q "$SHARED" -- sh -c 'test ! -e .private.env'
eq "a state-only check retains its state subject" state "$(l attestations "$SHARED" --kind check --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["subject"]["kind"])')"
A=$(l attest origin "views/$V" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["attestation_id"])')
eq "a view attestation revision resolves its shared state" "$SHARED" "$(l rev-parse "attestations/$A")"
ok "a view attestation can reproduce its stack" l try -q "attestations/$A" -- sh -c 'test "$(cat .private.env)" = private-alpha'
ok "branch from a view attestation preserves layers" l branch -q from-attestation "attestations/$A"
eq "attestation branch content" private-alpha "$(cat "$P/from-attestation/.private.env")"
l attest revoke "views/$V" >/dev/null
eq "a view revoke is recorded" 1 "$(l attestations "views/$V" --kind revoke --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
l op export > "$T/records.jsonl"
ok "view records do not include private content" python3 -c 'import json,sys; views=[json.loads(x)["r"]["data"] for x in open(sys.argv[1]) if json.loads(x)["r"]["op"]=="view.save"]; assert views and all("private-alpha" not in json.dumps(x) and "private-beta" not in json.dumps(x) for x in views)' "$T/records.jsonl"
ok "reindex keeps saved descriptors" l reindex -q
eq "a descriptor survives reindex" "$V" "$(l view show "$V" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')"
has "view records retain valid signatures" 'signatures ok' "$(l fsck)"

# Signed view records travel without private overlay contents.
new_fs "$T/peer" 2G
PEER=$T/peer/store
l remote add view-peer ssh://test/viewtest >/dev/null
ok "sync transfers saved view records" l sync view-peer -q --command -- env LAYR_ROOT="$PEER" "$LAYR" serve-peer viewtest
B() { env LAYR_ROOT="$PEER" "$LAYR" --project viewtest "$@"; }
eq "the peer resolves the shared view revision" "$SHARED" "$(B rev-parse "views/$V")"
eq "the peer reads the exact descriptor" "$V" "$(B view show "$V" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')"
no "the peer cannot materialize an unavailable private layer" B try -q "views/$V" -- true

# Reader rights apply to the full view, including --owner for a different user.
chmod 755 "$T" "$LAYR_ROOT"
for u in layr-view-reader layr-view-blocked; do id "$u" >/dev/null 2>&1 || useradd -m "$u"; done
export LAYR_SOCKET=$T/views.sock
cp "$LAYR" "$T/layr"; chmod 755 "$T/layr"
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
as_user() { local user=$1; shift; (cd /tmp; runuser -u "$user" -- env LAYR_SOCKET="$LAYR_SOCKET" LAYR_PROJECT=viewtest LAYR_LINE=main "$T/layr" "$@"); }
l access grant layr-view-reader writer
l access grant layr-view-blocked writer
ok "a non-reader can read a view descriptor" as_user layr-view-blocked view show "$V" --json
has "a non-reader cannot run a private view" "overlay 'private'" "$(as_user layr-view-blocked try -q "views/$V" -- true 2>&1)"
no "a non-reader cannot branch from a private view" as_user layr-view-blocked branch refused "views/$V" -q
no "a refused view branch leaves no folder" test -e "$P/refused"
no "a readable view cannot expose files to a non-reader owner" l branch expose "views/$V" --owner layr-view-blocked -q
l overlay readers private layr-view-reader
ok "an overlay reader can run a view" as_user layr-view-reader try -q "views/$V" -- sh -c 'test "$(cat .private.env)" = private-alpha'
ok "an overlay reader can branch from a view" as_user layr-view-reader branch reader-line "views/$V" -q
eq "a reader branch has exact private content" private-alpha "$(cat "$P/reader-line/.private.env")"
l overlay readers private root
no "revoked overlay access blocks an existing line's try" as_user layr-view-reader try -q reader-line -- true
kill "$DPID"; wait "$DPID" 2>/dev/null || true
unset LAYR_SOCKET
trap cleanup EXIT

# A saved descriptor is not a garbage-collection root.
cd "$T"
l init gcviews --from "$T/src" -q >/dev/null
G=$LAYR_ROOT/projects/gcviews/lines
cd "$G/main"
l config retention.auto 0; l config retention.hours 0; l config undo.days 0
echo temporary > code
l save -q
AUTO=$(l rev-parse 'HEAD@{0}')
l branch -q temporary "$AUTO"
cd "$G/temporary"
OLD=$(l view save --json)
OLD_ID=$(printf '%s' "$OLD" | python3 -c 'import json,sys; print(json.load(sys.stdin)["view_id"])')
OLD_STATE=$(printf '%s' "$OLD" | python3 -c 'import json,sys; print(json.load(sys.stdin)["view"]["shared"])')
cd "$G/main"
l branch -D temporary -q >/dev/null
l reset -q --hard HEAD
l ref set pins/view-old "views/$OLD_ID" >/dev/null
l gc -q
ok "a pin preserves the shared state of a view" l view "views/$OLD_ID" --json
l ref delete pins/view-old >/dev/null
l gc -q
has "full resolution names a collected shared state" "view $OLD_ID refers to a collected state $OLD_STATE" "$(l view "views/$OLD_ID" 2>&1)"
no "branch refuses a collected view" l branch gone "views/$OLD_ID" -q
no "try refuses a collected view" l try -q "views/$OLD_ID" -- true
ok "view show keeps the descriptor after collection" l view show "$OLD_ID" --json

finish
