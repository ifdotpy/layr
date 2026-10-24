#!/bin/bash
# Import a Desk v1 vault without writing to its source objects.
. "$(dirname "$0")/lib.sh"
set -e
REPO=$(cd "$(dirname "$0")/.." && pwd)
export LAYR_TEST_DESK_FIXTURE_DIR=$T/desk-vault LAYR_TEST_DESK_KEY_FILE=$T/desk-paper
"${CARGO:-$HOME/.cargo/bin/cargo}" test --manifest-path "$REPO/Cargo.toml" emit_desk_v1_fixture -- --ignored > "$T/fixture.log" 2>&1 || { tail -35 "$T/fixture.log"; exit 1; }
find "$T/desk-vault" -type f -exec sha256sum {} \; | sort > "$T/before"
export LAYR_PAPER_KEY_FILE=$T/desk-paper
l clone "file://$T/desk-vault" imported -q
cd "$LAYR_ROOT/projects/imported/lines/main"
eq "latest Desk content restores" 'Desk final state' "$(cat notes.txt)"
eq "Desk history has two import states" 2 "$(l log --format=%s | wc -l)"
has "oldest Desk state remains readable" 'Desk first state' "$(l show 'head~1:notes.txt')"
l op export > "$T/import.jsonl"
ok "signed import records preserve Desk sequence order" python3 - "$T/import.jsonl" <<'PY'
import json,sys
records=[json.loads(s)['r'] for s in open(sys.argv[1])]
imports=[r for r in records if r['op']=='import']
assert [r['data']['desk_seq'] for r in imports]==[1,2]
assert all(r['states'][0]['kind']=='import' for r in imports)
assert imports[1]['states'][0]['parents']==[imports[0]['states'][0]['id']]
attestations=[r for r in records if r['op']=='attestation' and r['data'].get('attestation',{}).get('kind')=='import']
assert len(attestations)==2
assert [r['data']['import']['unsigned'] for r in attestations]==[True,False]
assert [r['data']['attestation']['subject']['id'] for r in attestations]==[r['states'][0]['id'] for r in imports]
import hashlib
for record in attestations:
 evidence=record['data']['import']
 digest=hashlib.sha256(json.dumps(evidence,sort_keys=True,separators=(',',':')).encode()).hexdigest()
 assert record['data']['attestation']['payload_digest']==digest
PY
no "a Desk source rejects writes" l push origin -q
find "$T/desk-vault" -type f -exec sha256sum {} \; | sort > "$T/after"
ok "the source vault stays byte-identical" cmp "$T/before" "$T/after"
l remote add migrated "file://$T/v2" --paper-key-out "$T/v2-paper" >/dev/null
ok "imported history pushes into v2" l push migrated -q
ok "migrated history and all file frames verify" l fsck --remote migrated --files
# Build signed fixtures with a wrong header, missing signature, invalid signature or forged slot.
cd "$T"
for tamper in header missing signature slot; do
    export LAYR_TEST_DESK_FIXTURE_DIR=$T/desk-$tamper LAYR_TEST_DESK_KEY_FILE=$T/key-$tamper LAYR_TEST_DESK_TAMPER=$tamper
    "${CARGO:-$HOME/.cargo/bin/cargo}" test --manifest-path "$REPO/Cargo.toml" emit_desk_v1_fixture -- --ignored > "$T/fixture-$tamper.log" 2>&1 || { tail -35 "$T/fixture-$tamper.log"; exit 1; }
    export LAYR_PAPER_KEY_FILE=$T/key-$tamper
    no "Desk rejects $tamper" l clone "file://$T/desk-$tamper" "reject-$tamper" -q
    no "failed Desk import leaves no project for $tamper" test -d "$LAYR_ROOT/projects/reject-$tamper"
done
unset LAYR_TEST_DESK_TAMPER
export LAYR_TEST_DESK_FIXTURE_DIR=$T/relocated LAYR_TEST_DESK_KEY_FILE=$T/relocated-paper LAYR_TEST_DESK_PREFIX=original/prefix
"${CARGO:-$HOME/.cargo/bin/cargo}" test --manifest-path "$REPO/Cargo.toml" emit_desk_v1_fixture -- --ignored > "$T/fixture-prefix.log" 2>&1 || { tail -35 "$T/fixture-prefix.log"; exit 1; }
export LAYR_PAPER_KEY_FILE=$T/relocated-paper
no "Desk rejects another source prefix" l clone "file://$T/relocated" wrong-prefix -q
ok "a relocated Desk copy accepts its signed prefix" l clone "file://$T/relocated" relocated --desk-prefix original/prefix -q
finish
