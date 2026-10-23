#!/bin/bash
# Import a Desk v1 vault without writing to its source objects.
. "$(dirname "$0")/lib.sh"
set -e
export LAYR_TEST_DESK_FIXTURE_DIR=$T/desk-vault LAYR_TEST_DESK_KEY_FILE=$T/desk-paper
"${CARGO:-$HOME/.cargo/bin/cargo}" test emit_desk_v1_fixture -- --ignored > "$T/fixture.log" 2>&1 || { tail -35 "$T/fixture.log"; exit 1; }
find "$T/desk-vault" -type f -exec sha256sum {} \; | sort > "$T/before"
export LAYR_PAPER_KEY_FILE=$T/desk-paper
l clone "file://$T/desk-vault" imported -q
cd "$LAYR_ROOT/projects/imported/lines/main"
eq "latest Desk content restores" 'Desk final state' "$(cat notes.txt)"
eq "Desk history has two import states" 2 "$(l log --format=%s | wc -l)"
has "oldest Desk state remains readable" 'Desk first state' "$(l show 'HEAD~1:notes.txt')"
l op export > "$T/import.jsonl"
ok "signed import records preserve Desk sequence order" python3 - "$T/import.jsonl" <<'PY'
import json,sys
records=[json.loads(s)['r'] for s in open(sys.argv[1])]
imports=[r for r in records if r['op']=='import']
assert [r['data']['desk_seq'] for r in imports]==[1,2]
assert all(r['states'][0]['kind']=='import' for r in imports)
assert imports[1]['states'][0]['parents']==[imports[0]['states'][0]['id']]
PY
no "a Desk source rejects writes" l push origin -q
find "$T/desk-vault" -type f -exec sha256sum {} \; | sort > "$T/after"
ok "the source vault stays byte-identical" cmp "$T/before" "$T/after"
l remote add migrated "file://$T/v2" --paper-key-out "$T/v2-paper" >/dev/null
ok "imported history pushes into v2" l push migrated -q
ok "migrated history and all file frames verify" l fsck --remote migrated --files
finish
