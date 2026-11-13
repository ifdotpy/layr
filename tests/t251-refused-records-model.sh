#!/bin/bash
# ci: macos overlayfs
# Records that the trust rules refuse (docs/ci-trust.md) stay in the log but change no view. A reader machine signs a
# change close, a reaction, a thread resolve, a reviewer request and a queue result; after a fetch the other machine
# shows the change open, without the reaction, the thread open, no reviewer and its queue entry pending.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

js() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
l line new -q topic
cd "$P/topic"
echo one > one.txt
l add one.txt
l save -q -m "feat: one"
C=$(l log -n1 --json | head -1 | js "d['change_id']")
COMMENT=$(l comment "$C" -m "Why?" --json | js "d['id']")
cd "$P/main"
l line new -q queued
cd "$P/queued"
echo two > two.txt
l add two.txt
l save -q -m "feat: two"
D=$(l log -n1 --json | head -1 | js "d['change_id']")
ok "queue the second change without a run" l queue add queued --into main --no-run-queue
ok "remote add" l remote add cloud "file://$T/plain"
ok "push to the store" l push cloud -q

FIRST=$LAYR_ROOT
new_fs "$T/reader" 2G
export LAYR_ROOT=$T/reader/store
cd "$T"
ok "the reader clones the store" l clone "file://$T/plain" p -q
R=$LAYR_ROOT/projects/p/lines
read -r READER_ID READER_KEY < <(cd "$R/main" && l remote self --json | js "d['id'] + ' ' + d['public_key']")

export LAYR_ROOT=$FIRST
cd "$P/main"
ok "trust the reader with the reader role" l remote trust cloud "$READER_ID" --public-key "$READER_KEY" --role reader
ok "push the trust" l push cloud -q

export LAYR_ROOT=$T/reader/store
cd "$R/main"
ok "the reader fetches" l fetch origin -q
l queue run --into main >"$T/reader-queue.out" 2>&1
eq "the reader's own queue merged the second change" done "$(l queue list --into main --json | js "d[-1]['status']")" || cat "$T/reader-queue.out"
ok "the reader closes the change on its machine" l change close "$C" -m "by a reader"
ok "the reader reacts" l react "$C" +1
ok "the reader resolves the thread" l comment resolve "$COMMENT"
ok "the reader requests a review" l review request "$C" machine:"$READER_ID"
eq "the reader's own view takes its own records" closed "$(l change show "$C" --json | js "d['status']")"
ok "the reader pushes its records" l push origin -q

export LAYR_ROOT=$FIRST
cd "$P/main"
ok "fetch the reader's records" l fetch cloud -q
has "the reader's close is in the log" "change.close" "$(l op log --all --json)"
SHOW=$(l change show "$C" --json)
eq "a reader's close leaves the change open" open "$(echo "$SHOW" | js "d['status']")"
eq "a reader's reaction is not counted" 0 "$(echo "$SHOW" | js "sum(len(v) if isinstance(v, list) else v for v in (d.get('reactions') or {}).values())")"
eq "a reader's resolve leaves the thread open" False "$(echo "$SHOW" | js "next(t['resolved'] for t in d['threads'] if t['id'] == '$COMMENT')")"
eq "a reader's reviewer request is not counted" 0 "$(echo "$SHOW" | js "len(d.get('reviewers') or [])")"
has "change list shows the change" "$C" "$(l change list)"
eq "a reader's queue result does not merge the change" open "$(l change show "$D" --json | js "d['status']")"
eq "a reader's queue result leaves the entry pending" pending "$(l queue list --into main --json | js "d[-1]['status']")"
finish
