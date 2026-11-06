#!/bin/bash
# ci: macos overlayfs
# Project modes (docs/cloud-rfc.md 7.0). A Private or Public store keeps plain objects: no slots, no paper key, no
# age encryption; the server access list decides who writes, so a fetch takes every signed record of the store. A
# remote added or cloned with `--mode` refuses a store of another mode, and a Secret store keeps today's encryption.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"

refused() { l remote add bad "file://$T/bad" --mode private --paper-key-out "$T/paper" 2>&1; }
has "a Private store has no paper key" "no paper key" "$(refused || true)"
ok "remote add --mode private creates the store" l remote add cloud "file://$T/plain" --mode private
eq "the mode is recorded" private "$(l config --get cloud.remote.cloud.mode)"
has "store.json names the mode" '"mode":"private"' "$(cat "$T/plain/store.json")"
no "a Private store has no slots" test -e "$T/plain/slots"
ok "push to the Private store" l push cloud -q
no "no object is age encrypted" grep -rlq "age-encryption.org" "$T/plain"

# The clone is another machine: its own store.
FIRST=$LAYR_ROOT
export LAYR_ROOT=$T/second
cd "$T"
no "a clone that asks for Secret refuses the Private store" l clone "file://$T/plain" wrong --mode secret
ok "clone the Private store" l clone "file://$T/plain" copy -q
eq "the clone reads the files" "$(cat "$T/src/notes.txt")" "$(cat "$LAYR_ROOT/projects/copy/lines/main/notes.txt")"
cd "$LAYR_ROOT/projects/copy/lines/main"
eq "the clone records the mode" private "$(l config --get cloud.remote.origin.mode)"
no "the clone writes no machine slot" test -e "$T/plain/slots"
echo second >> notes.txt
ok "save in the clone" l save -a -m second -q
ok "push from the clone" l push origin -q

export LAYR_ROOT=$FIRST
cd "$LAYR_ROOT/projects/p/lines/main"
ok "fetch takes the records of the other machine without machine trust" l fetch cloud
has "the fetched change is listed" second "$(l change list)"

ok "remote add --mode secret creates an encrypted store" l remote add vault "file://$T/secret" --mode secret --paper-key-out "$T/paper-key"
ok "push to the Secret store" l push vault -q
ok "Secret objects stay age encrypted" grep -rlq "age-encryption.org" "$T/secret/states"
finish
