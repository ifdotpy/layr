#!/bin/bash
# A state from a non-btrfs store source passes the existing peer UUID check.
. "$(dirname "$0")/lib.sh"
set -e
export LAYR_TEST_PORTABLE_DIR=$T/portable LAYR_TEST_PORTABLE_KEY=$T/paper
"${CARGO:-$HOME/.cargo/bin/cargo}" test emit_portable_v2_fixture -- --ignored > "$T/fixture.log" 2>&1 || { tail -35 "$T/fixture.log"; exit 1; }
export LAYR_PAPER_KEY_FILE=$T/paper
l clone "file://$T/portable" project -q
unset LAYR_PAPER_KEY_FILE
cd "$LAYR_ROOT/projects/project/lines/main"
eq "portable state content restores" 'portable file bytes' "$(cat notes.txt)"
new_fs "$T/peer" 4G
BR=$T/peer/store
ok "portable state passes the peer's full-stream UUID check" peer_exchange sync project -q -- env LAYR_ROOT="$BR" "$LAYR" serve-peer project
ok "the peer verifies the signed history" env LAYR_ROOT="$BR" "$LAYR" --project project fsck
l op export > "$T/records"
ok "the peer receives exactly the portable source content" python3 - "$T/records" "$BR/projects/project" <<'PY'
import json,pathlib,sys
for line in open(sys.argv[1]):
    record=json.loads(line)['r']
    for state in record.get('states',[]):
        path=pathlib.Path(sys.argv[2])/'states'/state['id']/'notes.txt'
        assert path.read_bytes()==b'portable file bytes\n'
PY
finish
