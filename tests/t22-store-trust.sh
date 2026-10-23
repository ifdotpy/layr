#!/bin/bash
# Grant an unknown machine a slot and reject another key under its id.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init project --from "$T/source" -q >/dev/null
AR=$LAYR_ROOT
A=$AR/projects/project/lines/main
cd "$A"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l remote add reject "file://$T/reject" --paper-key-out "$T/reject-paper" >/dev/null
new_fs "$T/c" 4G
CR=$T/c/store
env LAYR_ROOT="$CR" "$LAYR" remote self --json > "$T/c-self"
read -r cm ck < <(python3 - "$T/c-self" <<'PY'
import json,sys
value=json.load(open(sys.argv[1]));print(value['id'],value['public_key'])
PY
)
ok "trust an unknown machine by its public key" l remote trust archive "$cm" --public-key "$ck"
l remote trust reject "$cm" --public-key "$ck" >/dev/null
l push archive -q
l push reject -q
export LAYR_ROOT=$CR
unset LAYR_PAPER_KEY_FILE
cd "$T"
ok "an unknown machine clones with its slot and no paper key" l clone "file://$T/archive" project -q
cd "$CR/projects/project/lines/main"
ok "the new machine can read its store" l fsck --remote origin --files
ok "the granted machine publishes records" l push origin -q
export LAYR_ROOT=$AR
cd "$A"
ok "the source accepts the granted key" l fetch archive -q
# Another machine retains its own signing key but claims the granted public machine id.
new_fs "$T/rogue" 4G
RR=$T/rogue/store
export LAYR_ROOT=$RR LAYR_PAPER_KEY_FILE=$T/reject-paper
cd "$T"
l clone "file://$T/reject" project -q
printf '%s' "$cm" > "$RR/machine/id"
cd "$RR/projects/project/lines/main"
l config test.mismatch yes
l push origin -q
export LAYR_ROOT=$AR
unset LAYR_PAPER_KEY_FILE
cd "$A"
rc=0
l fetch reject -q > "$T/rejected.out" 2> "$T/rejected.err" || rc=$?
no "another key under the granted id is refused" test "$rc" = 0
has "the refusal identifies the trusted-key mismatch" 'first-record key does not match' "$(cat "$T/rejected.err")"
finish
