#!/bin/bash
# ci: macos overlayfs
# Local Git recovery is preserved. Early selection validation is a deliberate error-path improvement.
. "$(dirname "$0")/lib.sh"

mkdir "$T/up"
git -C "$T/up" init -q -b main
printf 'local Git content\n' > "$T/up/payload"
git -C "$T/up" add payload
git -C "$T/up" commit -qm source
l git clone "$T/up" p -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null 2>&1
state=$(python3 - "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" <<'PYHEAD'
import sqlite3, sys
print(sqlite3.connect(sys.argv[1]).execute("SELECT head FROM lines WHERE name='main'").fetchone()[0])
PYHEAD
)
# Simulate the interrupted Git trim: the catalog still says present while the tree has gone.
state_dir="$LAYR_ROOT/projects/p/states/$state"
if [ "$(uname -s)" = Darwin ]; then
    chflags -R nouchg "$state_dir"; chmod -R -N "$state_dir"
elif [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    chmod -R u+w "$state_dir"
else
    btrfs property set -f -ts "$state_dir" ro false
fi
l __empty-trash "$LAYR_ROOT/projects/p/states"
# Only one imported state exists in this fixture. Its signed record and local Git objects remain.
ok "push rebuilds a present-but-missing imported tree from local Git" l push archive -q
ok "the local tree was rebuilt" test -f "$state_dir/payload"

# A valid Git commit-list object with a deliberately missing pack: selection must fail first.
# The v2 plain store fixture makes the requested-key ordering observable without encryption tooling.
mkdir -p "$T/plain/git"
python3 - "$T/plain" "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" <<'PY'
import json, pathlib, sqlite3, sys
out=pathlib.Path(sys.argv[1])
with sqlite3.connect(sys.argv[2]) as db:
    records=[row[0] for row in db.execute('SELECT raw FROM records ORDER BY machine, seq')]
project=json.loads(records[0])['r']['project']
(out/'store.json').write_text(json.dumps({'format':2,'project':project,'recipient':'plain','mode':'private'}))
(out/'git/deadbeef.commits.age').write_text(json.dumps({'format':1,'commits':['0'*40]}))
PY
l remote add plain "file://$T/plain" >/dev/null 2>&1
out=$(l fetch plain --line missing 2>&1); code=$?
no "invalid --line fails" test "$code" -eq 0
has "selection is validated before fetching Git packs" "--line: line 'missing' is not in the store" "$out"

# Trace a separate native source so the trace assertion is independent of Git recovery.
sample "$T/trace-source"
l init trace --from "$T/trace-source" -q >/dev/null
cd "$LAYR_ROOT/projects/trace/lines/main"
l remote add archive "file://$T/trace-archive" --paper-key-out "$T/trace-paper" >/dev/null 2>&1
# Trace spans are nested by lifetime: the whole-push span must finish after the final upload/checkpoint.
out=$(LAYR_TRACE=1 l push archive 2>&1); code=$?
eq "traced push succeeds" 0 "$code"
last=$(printf '%s\n' "$out" | sed -n 's/^trace .* ms  \(store push\|store load index\|store select history\)$/\1/p' | tail -1)
eq "store push spans the complete push" "store push" "$last"
finish
