#!/bin/bash
# A partial clone retains available lines and a later fetch creates the missing line. A line that
# waits for its head fails only a fetch that names it.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init project --from "$T/source" -q >/dev/null
AR=$LAYR_ROOT
cd "$AR/projects/project/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l line new -q delayed
cd ../delayed
printf 'delayed bytes\n' >> notes.txt
l save -qam delayed
l push archive -q
l op export > "$T/records"
state=$(python3 - "$T/records" <<'PY'
import json,sys
states=[s for line in open(sys.argv[1]) for s in json.loads(line)['r'].get('states',[]) if s.get('message')=='delayed']
print(states[-1]['id'])
PY
)
tree=$(find "$T/archive/states/$state" -name '*.tree.age' | head -1)
mv "$tree" "$T/unfinished-tree.age"
new_fs "$T/copy" 4G
export LAYR_ROOT=$T/copy/store LAYR_PAPER_KEY_FILE=$T/paper
cd "$T"
rc=0
l clone "file://$T/archive" project --json > "$T/partial.json" 2> "$T/partial.err" || rc=$?
eq "an incomplete clone returns partial status" 2 "$rc"
ok "the clone retains an available line" test -d "$LAYR_ROOT/projects/project/lines/main"
no "the clone creates no unavailable working folder" test -e "$LAYR_ROOT/projects/project/lines/delayed"
ok "JSON names the missing line and state" python3 - "$T/partial.json" "$state" <<'PY'
import json,sys
missing=json.load(open(sys.argv[1]))['missing_lines']
assert len(missing)==1
assert missing[0].startswith('line delayed: head state '+sys.argv[2])
assert 'layr fetch <remote>' in missing[0]
PY
unset LAYR_PAPER_KEY_FILE
# A plain fetch takes what is there: a line whose head tree is not on the remote yet is named on
# stderr, and the fetch succeeds (docs/remotes.md). A fetch that names the line with --line fails
# with the partial status until the tree exists.
rc=0
l --project project fetch origin > "$T/still-partial.txt" 2> "$T/still-partial.err" || rc=$?
eq "a plain fetch succeeds while another line waits" 0 "$rc"
has "stderr names the unavailable line" 'line delayed: head state' "$(cat "$T/still-partial.err")"
rc=0
l --project project fetch origin --line delayed > /dev/null 2> "$T/asked.err" || rc=$?
eq "fetch --line of that line retains partial status until the tree exists" 2 "$rc"
has "and names it" 'line delayed: head state' "$(cat "$T/asked.err")"
CR=$LAYR_ROOT
export LAYR_ROOT=$AR
cd "$AR/projects/project/lines/main"
ok "the writer completes its interrupted state" l push archive -q
export LAYR_ROOT=$CR
cd "$T"
ok "a later fetch completes the clone" l --project project fetch origin -q
has "fetch creates the missing working folder with exact bytes" 'delayed bytes' "$(cat "$CR/projects/project/lines/delayed/notes.txt")"
printf 'local edit\n' >> "$CR/projects/project/lines/delayed/notes.txt"
l --project project fetch origin -q
has "another fetch preserves local working edits" 'local edit' "$(cat "$CR/projects/project/lines/delayed/notes.txt")"
ok "the completed clone verifies" l --project project fsck
finish
