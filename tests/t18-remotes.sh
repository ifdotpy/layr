#!/bin/bash
# Named peer remotes, command migration, one-way exchange, pull and independent retention.
. "$(dirname "$0")/lib.sh"
set -e
l remote self --json > "$T/self.json"
ok "a host can show its identity outside a project" python3 - "$T/self.json" "$LAYR_ROOT/machine/id" <<'CHECK'
import json,pathlib,sys,uuid
d=json.load(open(sys.argv[1])); assert set(d)=={'id','public_key'}
assert d['id']==pathlib.Path(sys.argv[2]).read_text().strip(); uuid.UUID(d['id'])
assert len(bytes.fromhex(d['public_key']))==32
CHECK
sample "$T/src"
l init x --from "$T/src" -q >/dev/null
A=$LAYR_ROOT/projects/x
cd "$A/lines/main"
new_fs "$T/peer" 4G
BR=$T/peer/store
B() { env LAYR_ROOT="$BR" "$LAYR" --project x --line main "$@"; }
transport=(env LAYR_ROOT="$BR" "$LAYR" serve-peer x)

ok "add a named peer" l remote add peer ssh://test/x
no "reject a duplicate remote" l remote add peer ssh://test/x
has "Git URL needs the Git command" "layr git remote" "$(l remote add bad /tmp/repo.git 2>&1 || true)"
no "reject a host that starts with an option" l remote add bad ssh://-oCommand/x
no "reject a project with shell syntax" l remote add bad 'ssh://host/x;id'
l remote add registry oci://host/x
l remote list --json > "$T/list.json"
ok "the list has JSON remotes and machines" python3 - "$T/list.json" <<'CHECK'
import json,sys
d=json.load(open(sys.argv[1])); assert len(d['remotes'])==2 and len(d['machines'])==1
CHECK
l reindex >/dev/null
has "the signed configuration survives reindex" "ssh://test/x" "$(l remote list)"
l remote remove registry
l remote list --json > "$T/list.json"
ok "removal is persistent" python3 - "$T/list.json" <<'CHECK'
import json,sys
assert not any(r['name']=='registry' for r in json.load(open(sys.argv[1]))['remotes'])
CHECK
no "paper-key output requires a store URL" l remote add extra ssh://host/x --paper-key-out "$T/key"

# The default transport must construct ssh host layr serve-peer project.
mkdir "$T/transport"
cat > "$T/transport/ssh" <<EOF
#!/bin/bash
printf '%s\\n' "\$@" > '$T/ssh-args'
exec env LAYR_ROOT='$BR' '$LAYR' serve-peer x
EOF
chmod +x "$T/transport/ssh"
PATH="$T/transport:$PATH" l push peer --json > "$T/push.json"
eq "ssh has an option separator, host and quoted project command" "-- test layr serve-peer 'x'" "$(tr '\n' ' ' < "$T/ssh-args" | sed 's/ $//')"
ok "push receives no records or states" python3 - "$T/push.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); assert d['records_received']==0 and d['states_received']==0
PY
B adopt main >/dev/null
cd "$BR/projects/x/lines/main"
echo peer > peer.txt
B add peer.txt
B commit -qm peer-first
bhead=$(B rev-parse HEAD)
cd "$A/lines/main"
ahead=$(l rev-parse HEAD)
l fetch peer --command -- "${transport[@]}"
eq "fetch leaves the local head" "$ahead" "$(l rev-parse HEAD)"
eq "fetch receives the peer state" peer "$(l show "$bhead:peer.txt")"
l pull peer --ff-only --json --command -- "${transport[@]}" > "$T/pull.json"
ok "pull prints one JSON result" python3 - "$T/pull.json" <<'CHECK'
import json,sys
d=json.load(open(sys.argv[1])); assert d['operation']=='pull' and d['conflicts']==0 and d['exit']==0
CHECK
eq "pull fast-forwards" "$bhead" "$(l rev-parse HEAD)"

# Each direction also limits records, including metadata, rather than only stream payloads.
echo local > local.txt
l add local.txt
l commit -qm local-only
local_head=$(l rev-parse HEAD)
cd "$BR/projects/x/lines/main"
echo second > second.txt
B add second.txt
B commit -qm peer-second
peer_head=$(B rev-parse HEAD)
cd "$A/lines/main"
l fetch peer --json --command -- "${transport[@]}" > "$T/fetch.json"
no "fetch does not send the local state record" B show "$local_head:local.txt"
ok "fetch sends no records or states" python3 - "$T/fetch.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); assert d['records_sent']==0 and d['states_sent']==0
PY
l push peer --command -- "${transport[@]}"
eq "push sends the local state" local "$(B show "$local_head:local.txt")"
# Make another peer state after fetch, so a push must not import it.
cd "$BR/projects/x/lines/main"
echo third > third.txt
B add third.txt
B commit -qm peer-third
third=$(B rev-parse HEAD)
cd "$A/lines/main"
l push peer --command -- "${transport[@]}"
no "push does not receive the peer state record" l show "$third:third.txt"
l pull peer --command -- "${transport[@]}"
eq "divergent pull makes a merge" 2 "$(l show -s --format=%p HEAD | wc -w | xargs)"
ok "the merge keeps both sides" test -f local.txt -a -f third.txt

# A conflicting pull uses the existing merge engine and keeps its conflict state.
cd "$BR/projects/x/lines/main"
echo theirs > notes.txt
B commit -qam peer-conflict
cd "$A/lines/main"
echo ours > notes.txt
l commit -qam local-conflict
no "a conflict makes pull fail" l pull peer --command -- "${transport[@]}"
has "the conflict remains in the line" "UU notes.txt" "$(l status -s)"
echo resolved > notes.txt
l add notes.txt
l commit -qm resolve

# Retention invariants use an unadopted line on B, so its deletion can be undone there.
l branch -q doomed
cd "$A/lines/doomed"
echo retained > retained.txt
l add retained.txt
l commit -qm retained
retained=$(l rev-parse HEAD)
retained_dir=$(ls -d "$A/states/"*"-${retained:0:12}")
cd "$A/lines/main"
l sync peer --command -- "${transport[@]}"
ls "$BR/projects/x/states" | sort > "$T/before-states"
echo uncommitted > "$A/lines/doomed/uncommitted.txt"
l branch -D doomed >/dev/null
deleted=$(l op log --all -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
l sync peer --command -- "${transport[@]}"
ls "$BR/projects/x/states" | sort > "$T/after-states"
eq "sync never removes a peer state" "" "$(comm -23 "$T/before-states" "$T/after-states")"
eq "the deleted line is absent on B" "" "$(B branch | grep doomed || true)"
l config undo.days 0
l config retention.auto 0
l config retention.hours 0
l gc >/dev/null
no "local gc removes the unreachable local state" test -e "$retained_dir"
ok "local gc leaves the other snapshot" test -d "$BR/projects/x/states/$(basename "$retained_dir")"
# Send the signed gc record. B must keep its own bytes and ignore the foreign deletion flag.
l sync peer --command -- "${transport[@]}"
eq "the peer still reads the state after the foreign gc record" retained "$(B show "$retained:retained.txt")"
undo_rc=0
B undo "$deleted" > "$T/undo.log" 2>&1 || undo_rc=$?
cat "$T/undo.log"
eq "undo on the other machine restores the deleted line" 0 "$undo_rc"
eq "undo restores its content" retained "$(cat "$BR/projects/x/lines/doomed/retained.txt")"
eq "undo restores the final uncommitted content" uncommitted "$(cat "$BR/projects/x/lines/doomed/uncommitted.txt")"
l sync peer --json --command -- "${transport[@]}" > "$T/repeat-sync.json"
ok "sync after recovery succeeds and transfers no states" python3 - "$T/repeat-sync.json" <<'CHECK'
import json,sys
d=json.load(open(sys.argv[1])); assert d['states_sent']==0 and d['states_received']==0
CHECK
l reindex >/dev/null
eq "reindex keeps the recovered snapshot readable" retained "$(l show "$retained:retained.txt")"
ok "sync can repeat after reindex of recovered states" l sync peer --command -- "${transport[@]}"
B remote self --json > "$T/peer-self.json"
peer_public_key=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["public_key"])' "$T/peer-self.json")
eq "trust accepts the same public key in uppercase" 0 "$(l remote trust peer "$(cat "$BR/machine/id")" --public-key "$(echo "$peer_public_key" | tr 'a-f' 'A-F')" >/dev/null; echo $?)"
ok "remote trust records the known key" l remote list --json

# Live refs make automatic states explicit transfer roots without --auto.
echo pinned-push > ref-auto.txt
l save -q
auto_push=$(l rev-parse 'HEAD@{0}')
l ref set pins/peer-push "$auto_push" >/dev/null
l push peer --command -- "${transport[@]}" >/dev/null
eq "push includes a live ref to an automatic state" pinned-push "$(B show "$auto_push:ref-auto.txt")"
cd "$BR/projects/x/lines/main"
echo pinned-fetch > ref-auto.txt
B save -q
auto_fetch=$(B rev-parse 'HEAD@{0}')
B ref set pins/peer-fetch "$auto_fetch" >/dev/null
cd "$A/lines/main"
l fetch peer --command -- "${transport[@]}" >/dev/null
eq "fetch includes a live ref to an automatic state" pinned-fetch "$(l show "$auto_fetch:ref-auto.txt")"
echo pinned-sync > ref-auto.txt
l save -q
auto_sync=$(l rev-parse 'HEAD@{0}')
l ref set pins/peer-sync "$auto_sync" >/dev/null
l sync peer --command -- "${transport[@]}" >/dev/null
eq "sync includes a live ref to an automatic state" pinned-sync "$(B show "$auto_sync:ref-auto.txt")"

has "old command transport names sync remote" "layr sync <remote>" "$(l sync -- false 2>&1 || true)"
has "old machines form names remote list" "layr remote list" "$(l machines 2>&1 || true)"
has "old Git sync form names git sync" "layr git sync" "$(l git-sync 2>&1 || true)"
retired=$(printf '\142\141\143\153\165\160')
has "the removed whole-project command is unknown" "is not a layr command" "$(l "$retired" 2>&1 || true)"
has "old fetch form names git fetch" "layr git" "$(l fetch origin 2>&1 || true)"
no "the internal peer command stays out of help" sh -c '"$1" help | grep -q serve-peer' sh "$LAYR"
finish
