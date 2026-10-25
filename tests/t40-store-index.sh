#!/bin/bash
# Index writer, retries, authenticated readers, and CLI history goldens.
. "$(dirname "$0")/lib.sh"
REPO=$(builtin cd "$(dirname "$0")/.." && pwd)
state_id() { l log "$1" -n 1 --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])'; }
mkdir -p "$T/src"
printf 'first\nshared\n' > "$T/src/file.txt"
printf 'unrelated\n' > "$T/src/other.txt"
l init indexed --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/indexed/lines/main"
chmod +x file.txt
l save -qam executable
printf 'last\nshared\n' > file.txt
l save -qam edit
EDIT=$(state_id head)
printf 'unrelated edit\n' > other.txt
l save -qam unrelated
UNCHANGED=$(state_id head)
rm file.txt
l save -qam delete
printf 'reborn\n' > file.txt
l add file.txt
l save -qm recreate
RECREATED=$(state_id head)
l line new feature -q
cd ../feature
printf 'branch\n' > file.txt
l save -qam feature
cd ../main
printf 'main side\n' > main.txt
l add main.txt
l save -qm main-side
l merge --no-ff -q feature >/dev/null
MERGE=$(state_id head)
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" -q
ok 'push publishes the index after records and trees' l push archive --index -q
ok 'the store contains all three encrypted index kinds' python3 - "$T/archive" <<'PY'
from pathlib import Path
import sys
root=Path(sys.argv[1])/'index'
assert all(list((root/kind).rglob('*.age')) for kind in ('heads','paths','graph'))
PY
BEFORE=$(find "$T/archive/index" -type f -exec sha256sum {} + | sort | sha256sum)
REPORT=$(l index write archive --json)
eq 'repeated write creates no objects' '0 0 0' "$(printf '%s' "$REPORT" | python3 -c 'import json,sys;x=json.load(sys.stdin);assert not x["unavailable_paths"];print(x["heads"],x["paths"],x["graphs"])')"
eq 'repeated write preserves all encrypted bytes' "$BEFORE" "$(find "$T/archive/index" -type f -exec sha256sum {} + | sort | sha256sum)"
rm "$T/archive/index/paths/$EDIT.age"
eq 'write fills a missing immutable path object only' '0 1 0' "$(l index write --json | python3 -c 'import json,sys;x=json.load(sys.stdin);print(x["heads"],x["paths"],x["graphs"])')"
rm "$T/archive/index/graph/$EDIT.age"
eq 'write fills a missing immutable graph object only' '0 0 1' "$(l index write archive --json | python3 -c 'import json,sys;x=json.load(sys.stdin);print(x["heads"],x["paths"],x["graphs"])')"
no 'index rejects an unsupported subcommand' l index read archive
no 'fetch cannot use the push index flag' l fetch archive --index

for ENTRY in "edit:$EDIT" "unchanged:$UNCHANGED" "recreated:$RECREATED" "merge:$MERGE"; do
    LABEL=${ENTRY%%:*}; STATE=${ENTRY#*:}
    mkdir -p "$T/$LABEL"
    l log "$STATE" --first-parent --json -- file.txt > "$T/$LABEL/cli-history.jsonl"
    l blame -s "$STATE" -- file.txt > "$T/$LABEL/cli-blame.txt"
    ok "$LABEL native index reader verifies the real store" bash -c 'cd "$1" && cargo run --locked -q -p layr-model --example index_golden -- "$2" "$3" "$4" "$5" file.txt' _ "$REPO" "$T/archive" "$T/paper" "$T/$LABEL" "$STATE"
    ok "$LABEL indexed file history equals the CLI" python3 - "$T/$LABEL" <<'PY'
from pathlib import Path
import sys,json
p=Path(sys.argv[1]); expected=[json.loads(line) for line in (p/'cli-history.jsonl').read_text().splitlines()]
assert expected==json.loads((p/'history.json').read_text())
PY
    ok "$LABEL indexed blame equals the CLI" python3 - "$T/$LABEL" <<'PY'
from pathlib import Path
import sys,json,uuid
p=Path(sys.argv[1]); actual=json.loads((p/'blame.json').read_text()); expected=(p/'cli-blame.txt').read_text().splitlines()
assert len(actual)==len(expected)
for a,e in zip(actual,expected):
    raw=uuid.UUID(a['state']).bytes; display=(raw[10:]+raw[6:10]+raw[:6]).hex()
    prefix='^'+display[:7] if a['boundary'] else display[:8]
    assert e.split()[0]==prefix,(a,e)
    assert e.split(') ',1)[1]==a['text'],(a,e)
PY
done
ok 'blame skips the unrelated state blob' python3 - "$T/unchanged/blob-reads.json" "$UNCHANGED" <<'PY'
import json,sys
assert sys.argv[2] not in json.load(open(sys.argv[1]))
PY
finish
