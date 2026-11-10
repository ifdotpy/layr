#!/bin/bash
# ci: macos overlayfs
# Project modes (docs/cloud-rfc.md 7.0). A Private or Public store keeps plain objects: no slots, no paper key, no
# age encryption. A new store is Private unless it asks for a paper key. On a bare folder or bucket there is no
# server access list, so a fetch still takes records of trusted machines only. A remote added or cloned with `--mode`
# refuses a store of another mode, the pin keeps the mode, and a Secret store keeps today's encryption.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"

refused() { l remote add bad "file://$T/bad" --mode private --paper-key-out "$T/paper" 2>&1; }
has "a Private store has no paper key" "no paper key" "$(refused || true)"
ok "remote add without a mode creates a Private store" l remote add cloud "file://$T/plain"
eq "the mode is recorded" private "$(l config --get cloud.remote.cloud.mode)"
has "store.json names the mode" '"mode":"private"' "$(cat "$T/plain/store.json")"
no "a Private store has no slots" test -e "$T/plain/slots"
ok "push to the Private store" l push cloud -q
no "no object is age encrypted" grep -rlq "age-encryption.org" "$T/plain"

# The clone is another machine: its own store, on its own file system (btrfs on Linux).
FIRST=$LAYR_ROOT
new_fs $T/second 2G
export LAYR_ROOT=$T/second/store
cd "$T"
no "a clone that asks for Secret refuses the Private store" l clone "file://$T/plain" wrong --mode secret
ok "clone the Private store" l clone "file://$T/plain" copy -q
eq "the clone reads the files" "$(cat "$T/src/notes.txt")" "$(cat "$LAYR_ROOT/projects/copy/lines/main/notes.txt")"
cd "$LAYR_ROOT/projects/copy/lines/main"
eq "the clone records the mode" private "$(l config --get cloud.remote.origin.mode)"
no "the clone writes no machine slot" test -e "$T/plain/slots"
read -r SECOND_MACHINE SECOND_KEY < <(l remote list --json | python3 -c 'import json,sys; m=next(m for m in json.load(sys.stdin)["machines"] if m["current"]); print(m["id"], m["key"])')
echo second >> notes.txt
ok "save in the clone" l save -a -m second -q
ok "push from the clone" l push origin -q

export LAYR_ROOT=$FIRST
cd "$LAYR_ROOT/projects/p/lines/main"
# A bare folder has no access list: the records of an unknown machine wait for trust.
has "fetch refuses records of an untrusted machine" "untrusted machine" "$(l fetch cloud 2>&1 || true)"
no "the change of the untrusted machine is not imported" sh -c "'$LAYR' change list | grep -q second"
ok "trust the other machine" l remote trust cloud "$SECOND_MACHINE" --public-key "$SECOND_KEY"
ok "fetch takes its records after trust" l fetch cloud
has "the fetched change is listed" second "$(l change list)"
no "trust writes no slot into a Private store" test -e "$T/plain/slots"

# A Secret pin keeps its mode: a store.json rewritten as plain is refused.
ok "remote add --mode secret creates an encrypted store" l remote add vault "file://$T/secret" --mode secret --paper-key-out "$T/paper-key"
ok "push to the Secret store" l push vault -q
ok "Secret objects stay age encrypted" grep -rlq "age-encryption.org" "$T/secret/states"
python3 - "$T/secret/store.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d["mode"] = "private"; d["recipient"] = "plain"
open(sys.argv[1], "w").write(json.dumps(d))
PY
has "a Secret store rewritten as plain is refused" "store mode" "$(l fetch vault 2>&1 || true)"
finish
