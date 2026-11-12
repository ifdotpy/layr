#!/bin/bash
# ci: macos overlayfs
# `save --json` prints one object for each form of save (-m, -am, --amend, automatic, --overlays),
# and "nothing to save" as an object with the exit status of the text. The JSON of change show,
# change list, queue and status names each line next to its id; a deleted line keeps its name.
. "$(dirname "$0")/lib.sh"

# py <description> <json> <python expression on j>: the expression is true for the parsed JSON.
py() {
    ok "$1" python3 -c 'import json,sys; j=json.loads(sys.argv[1]); assert eval(sys.argv[2]), j' "$2" "$3"
}
uuid() { l log "$1" -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }

sample $T/src
printf 'target regenerable\n' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
l line new -q w >/dev/null
cd $P/w || exit 1
W_ID=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"] == "w"))')
BASE=$(uuid head)

# save -m --json: the new state, its line, message, parents and the numbers of the summary.
echo more >> notes.txt; l add notes.txt >/dev/null
out=$(l save -m "feat: one" --json 2>/dev/null)
ONE=$(uuid head)
py "save -m --json prints the new state" "$out" "j['saved'] is True and j['state'] == '$ONE' and j['amend'] is False"
py "the short id is the one of the text" "$out" "j['short'] == '$(l rev-parse head | cut -c1-7)'"
py "the line has its id and name" "$out" "j['line'] == {'id': '$W_ID', 'name': 'w'}"
py "the message and the parents" "$out" "j['message'] == 'feat: one' and j['parents'] == ['$BASE']"
py "the files of the summary" "$out" "j['files'] == {'changed': 1, 'insertions': 1, 'deletions': 0} and j['conflicts'] == 0"
eq "the JSON is one line" 1 "$(printf '%s\n' "$out" | wc -l | tr -d ' ')"

# Nothing to save: an object, and exit status 1 as the text.
out=$(l save -m "feat: none" --json 2>/dev/null); rc=$?
eq "nothing to save exits 1" 1 "$rc"
py "nothing to save is an object" "$out" "j == {'saved': False, 'state': None, 'short': None, 'line': {'id': '$W_ID', 'name': 'w'}, 'reason': 'nothing to save'}"
no "nothing to save fails in text too" l save -m "feat: none"

# save -am --json takes the changed tracked files.
echo two >> notes.txt; printf 'x\n' > src/main.rs
out=$(l save -am "feat: two" --json 2>/dev/null)
TWO=$(uuid head)
py "save -am --json" "$out" "j['state'] == '$TWO' and j['parents'] == ['$ONE'] and j['files']['changed'] == 2 and j['files']['deletions'] == 3"

# save --amend --json replaces the head: the parents are those of the old head.
echo three >> notes.txt; l add notes.txt >/dev/null
out=$(l save --amend -m "feat: two, amended" --json 2>/dev/null)
AMENDED=$(uuid head)
py "save --amend --json" "$out" "j['state'] == '$AMENDED' and j['amend'] is True and j['parents'] == ['$ONE'] and j['message'] == 'feat: two, amended'"

# Text output stays as it was.
echo four >> notes.txt; l add notes.txt >/dev/null
text=$(l save -m "feat: four" 2>/dev/null)
has "the text names line, short id and subject" "[w $(l rev-parse head | cut -c1-7)] feat: four" "$text"
has "the text has the summary" "1 file changed, 1 insertion(+)" "$text"

# The automatic save: the state of the folder, new or not.
echo auto >> notes.txt
out=$(l save --json)
py "save --json makes an automatic state" "$out" "j['saved'] is True and j['line']['name'] == 'w' and j['overlays'] == [] and len(j['short']) == 7"
AUTO=$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')
out=$(l save --json)
py "an unchanged folder names the same state" "$out" "j['saved'] is False and j['state'] == '$AUTO'"

# save --overlays --json lists the cache overlays it saved.
rm -rf target; mkdir -p target; echo bin > target/out.bin
out=$(l save --overlays --json)
py "save --overlays --json names the cache overlay" "$out" "[o['path'] for o in j['overlays']] == ['target'] and all(len(o['short']) == 7 for o in j['overlays'])"
eq "save --overlays --json prints one line" 1 "$(printf '%s\n' "$out" | wc -l | tr -d ' ')"

# A line with an overlay: the object also names the overlay states.
echo secret > priv.txt
l overlay create priv -q
l overlay move priv priv.txt >/dev/null
out=$(l save -m "feat: priv" --json 2>/dev/null)
py "save -m --json in a line with an overlay" "$out" "j['saved'] is True and j['line']['name'] == 'w' and [o['overlay'] for o in j['overlays']] == ['priv']"

# change show and change list: the line name next to the line id.
CHANGE=$(l change list --json | python3 -c 'import json,sys; print(next(c["change_id"] for c in json.load(sys.stdin) if c["message"] == "feat: two, amended"))')
out=$(l change show "$CHANGE" --json)
py "change show names the line of each version" "$out" "all(v['state']['line'] == '$W_ID' and v['state']['line_name'] == 'w' for v in j['versions'])"
py "change show names the target line of the amend" "$out" "j['versions'][1]['operation']['target_line'] == '$W_ID' and j['versions'][1]['operation']['target_line_name'] == 'w'"
out=$(l change list --json)
py "change list names the line" "$out" "all(c['line'] == '$W_ID' and c['line_name'] == 'w' for c in j if c['message'].startswith('feat:'))"

# status --json: the line id next to the line name.
out=$(l status --json)
py "status --json has the line id" "$out" "j['line'] == 'w' and j['line_id'] == '$W_ID'"

# queue add and queue list: the name of the target line.
MAIN_ID=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"] == "main"))')
out=$(l queue add w --into main --no-run-queue --json)
py "queue add names the target line" "$out" "j['target_line'] == '$MAIN_ID' and j['target_line_name'] == 'main'"
out=$(l queue list --json)
py "queue list names the target line" "$out" "j[0]['target_line'] == '$MAIN_ID' and j[0]['target_line_name'] == 'main'"

# A deleted line keeps its name in the JSON.
cd $P/main || exit 1
l line new -q gone >/dev/null
cd $P/gone || exit 1
GONE_ID=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"] == "gone"))')
echo gone > gone.txt; l add gone.txt >/dev/null; l save -qm "feat: gone"
cd $P/main || exit 1
l line delete -D gone >/dev/null 2>&1
GONE_CHANGE=$(l change list --json | python3 -c 'import json,sys; print(next(c["change_id"] for c in json.load(sys.stdin) if c["message"] == "feat: gone"))')
out=$(l change show "$GONE_CHANGE" --json)
py "a deleted line keeps its name" "$out" "j['versions'][0]['state']['line'] == '$GONE_ID' and j['versions'][0]['state']['line_name'] == 'gone'"
finish
