#!/bin/bash
# Derived files, batch lines, saved-state radar, and safe hard-reset snapshots.
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src"
printf 'first\nsecond\nthird\n' > "$T/src/input"
echo original > "$T/src/middle"
echo original > "$T/src/final"
printf 'middle derive=z\nfinal derive=a\n' > "$T/src/.layrattributes"
l init cloud-merge --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/cloud-merge/lines
cd "$P/main"
l config derive.command.z 'cat input > middle; echo discarded > extra'
l config derive.inputs.z input
l config derive.command.a 'cat middle > final'
l config derive.inputs.a middle
BASE=$(l rev-parse head)
l line new -q topic head
sed -i 's/first/ours/' input
echo ours > middle
l save -qam ours
cd "$P/topic"
sed -i 's/third/theirs/' input
echo theirs > middle
l save -qam theirs
cd "$P/main"
ok "derive merges without a lockfile conflict" l merge -q topic
ok "derive receives both sides of the merged input" sh -c 'grep -q ours middle && grep -q theirs middle'
eq "dependent rule receives the first output" "$(cat middle)" "$(cat final)"
no "unrelated command output is discarded" test -e extra
no "derived output has no line markers" grep -q '^<<<<<<<' middle

# A failing command keeps ours, and the state names the conflict.
l line new -q failure head
l config derive.command.z 'echo bad > middle; exit 1'
OURS=$(cat middle)
echo local > local; l add local; l save -qm local
cd "$P/failure"; echo changed > input; l save -qam changed
cd "$P/main"
no "derive failure returns a conflict" l merge -q failure
eq "derive failure keeps ours" "$OURS" "$(cat middle)"
has "state records derive failure" 'derive z failed' "$(l status --json)"
has "dependent command is also a conflict" 'derive a failed' "$(l status --json)"
l reset -q --hard head~1
l config derive.command.z 'cat input > middle'

# Cherry-pick and rebase use the same tree engine.
l line new -q pick "$BASE"
cd "$P/pick"; echo picked > input; l save -qam picked
PICK=$(l rev-parse head)
cd "$P/main"; l reset -q --hard "$BASE"
ok "cherry-pick regenerates derived files" l pick -q "$PICK"
eq "cherry-pick output" picked "$(cat final)"
l line new -q rebased "$BASE"
cd "$P/rebased"; echo side > side; l add side; l save -qm side
ok "rebase regenerates derived files" l replay -q main
eq "rebase output" picked "$(cat final)"

# A fast-forward keeps the exact target, even when its derived file is stale.
cd "$P/main"
l line new -q ff-source head
cd "$P/ff-source"; echo stale-input > input; l save -qam stale-derived
FF=$(l rev-parse head)
cd "$P/main"
l config derive.command.z false
ok "fast-forward keeps the exact reviewed state" l merge -q ff-source
eq "fast-forward head matches the target" "$FF" "$(l rev-parse head)"
eq "fast-forward keeps the target derived file" picked "$(cat final)"
l config derive.command.z 'cat input > middle'

# Cycles are conflicts, with ours retained.
cd "$P/main"
l line new -q cycle head
echo independent > independent; l add independent; l save -qm independent
cd "$P/cycle"; echo trigger > input; l save -qam trigger
cd "$P/main"
l config derive.inputs.z final
OURS=$(cat middle)
no "derive dependency cycle is detected" l merge -q cycle
has "cycle is stored in the state" 'derive dependency cycle' "$(l status --json)"
eq "cycle keeps ours" "$OURS" "$(cat middle)"
l reset -q --hard head~1
l config derive.inputs.z input

# A literal output can be missing from all three trees.
printf 'middle derive=z\nfinal derive=a\nnew-output derive=new\n' > .layrattributes
l config derive.command.new 'cat input > new-output'
l config derive.inputs.new input
l save -qam attributes
l line new -q absent head
echo m > m; l add m; l save -qm m
cd "$P/absent"; echo absent > input; l save -qam absent
cd "$P/main"
ok "derive creates a declared absent file" l merge -q absent
eq "absent output content" absent "$(cat new-output)"

# The merged attributes select the command, including an incoming rule change.
l line new -q incoming-attributes head
echo independent > attr-local; l add attr-local; l save -qm attr-local
cd "$P/incoming-attributes"
sed -i 's/middle derive=z/middle derive=incoming/' .layrattributes
echo incoming-input > input
l save -qam incoming-attributes
cd "$P/main"
l config derive.command.incoming 'printf incoming-rule > middle'
l config derive.inputs.incoming input
ok "derive uses the merged attribute rule" l merge -q incoming-attributes
eq "incoming rule replaces the old command" incoming-rule "$(cat middle)"
l reset -q --hard head~1

# Batch creation uses ordinary per-line records and validates every name first.
BEFORE=$(l op log --all --json -n 1000 | python3 -c 'import sys,json; print(sum(json.loads(x)["op"]=="line.create" for x in sys.stdin))')
JSON=$(l line new trial head --count 3 --json)
eq "batch JSON contains numbered lines" 'trial-1 trial-2 trial-3' "$(printf '%s' "$JSON" | python3 -c 'import json,sys; print(" ".join(x["name"] for x in json.load(sys.stdin)))')"
AFTER=$(l op log --all --json -n 1000 | python3 -c 'import sys,json; print(sum(json.loads(x)["op"]=="line.create" for x in sys.stdin))')
eq "batch writes one record per line" 3 "$((AFTER - BEFORE))"
no "zero count fails" l line new zero head --count 0
no "invalid count fails" l line new invalid head --count nope
no "batch refuses an existing destination" l line new trial head --count 4
no "batch conflict creates no later line" test -e "$P/trial-4"

# A database failure after the first record keeps only that complete line.
DB=$LAYR_ROOT/projects/cloud-merge/meta/db/layr.sqlite
python3 - "$DB" <<'PYDB'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.execute("CREATE TRIGGER fail_batch BEFORE INSERT ON records WHEN json_extract(NEW.raw, '$.r.data.name') = 'partial-2' BEGIN SELECT RAISE(ABORT, 'injected record failure'); END")
PYDB
out=$(l line new partial head --count 3 2>&1); rc=$?
eq "batch stops on a record failure" 128 "$rc"
has "batch reports completed lines after failure" 'created lines: partial-1' "$out"
ok "completed line still has a snapshot" test -f "$P/partial-1/input"
ok "completed line still has a record" l rev-parse partial-1
no "failed line has no folder" test -e "$P/partial-2"
no "unrecorded line has no folder" test -e "$P/partial-3"
no "failed line has no record" l rev-parse partial-2
python3 - "$DB" <<'PYDB'
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.execute("DROP TRIGGER fail_batch")
PYDB

# Radar compares this live content with other lines' newest saved states.
l line new -q radar head
cd "$P/radar"; echo other > input; l save -q
cd "$P/main"; echo current > input
JSON=$(l status --json)
ok "radar lists the path and the other line" python3 -c 'import json,sys; a=json.loads(sys.argv[1]); assert any(x["path"]=="input" and x["line"]=="radar" and isinstance(x["owner"],int) and x["changed_at"]>0 for x in a["overlaps"])' "$JSON"
has "long status reports radar" 'Overlaps with other live lines:' "$(l status)"
cd "$P/radar"; l reset -q --hard head
cd "$P/main"
JSON=$(l status --json)
ok "reset clears a saved overlap" python3 -c 'import json,sys; assert not any(x["line"]=="radar" for x in json.loads(sys.argv[1])["overlaps"])' "$JSON"
l reset -q --hard head

# A reset from outside the line can exchange the root subvolume.
l line new -q reset "$BASE"
btrfs subvolume create "$P/reset/.git" >/dev/null
echo retained > "$P/reset/.git/custom"
echo dirty > "$P/reset/input"
UUID=$(btrfs subvolume show "$P/reset" | awk '/^[[:space:]]*UUID:/{print $2}')
cd "$T"
ok "hard reset outside an idle line succeeds" l --project cloud-merge --line reset reset -q --hard "$BASE"
NEXT=$(btrfs subvolume show "$P/reset" | awk '/^[[:space:]]*UUID:/{print $2}')
no "idle reset swaps the root snapshot" test "$UUID" = "$NEXT"
eq "snapshot reset restores content" "$(cat "$T/src/input")" "$(cat "$P/reset/input")"
eq "snapshot reset keeps Git local data" retained "$(cat "$P/reset/.git/custom")"
echo untracked > "$P/reset/untracked"
UUID=$NEXT
l --project cloud-merge --line reset reset -q --hard "$BASE"
eq "untracked file survives hard reset" untracked "$(cat "$P/reset/untracked")"
eq "untracked files force the copy path" "$UUID" "$(btrfs subvolume show "$P/reset" | awk '/^[[:space:]]*UUID:/{print $2}')"
rm "$P/reset/untracked"
cd "$P/reset"
UUID=$(btrfs subvolume show . | awk '/^[[:space:]]*UUID:/{print $2}')
l reset -q --hard "$BASE"
eq "a caller inside the line keeps its directory" "$UUID" "$(btrfs subvolume show . | awk '/^[[:space:]]*UUID:/{print $2}')"

# An open file in another process makes a snapshot swap unsafe.
cd "$T"
python3 - "$P/reset/input" "$T/ready" <<'PYHOLD' &
import sys, time
with open(sys.argv[1], "r+") as stream:
    open(sys.argv[2], "w").close()
    time.sleep(30)
PYHOLD
HOLDER=$!
for i in $(seq 1 50); do [ -e "$T/ready" ] && break; sleep 0.1; done
UUID=$(btrfs subvolume show "$P/reset" | awk '/^[[:space:]]*UUID:/{print $2}')
l --project cloud-merge --line reset reset -q --hard "$BASE"
eq "an open file forces the copy path" "$UUID" "$(btrfs subvolume show "$P/reset" | awk '/^[[:space:]]*UUID:/{print $2}')"
kill "$HOLDER"; wait "$HOLDER" 2>/dev/null || true

# Overlay paths use their own common base. Shared bases contain no private files.
mkdir -p "$T/overlay-src"
echo original > "$T/overlay-src/shared"
l init radar-overlays --from "$T/overlay-src" -q >/dev/null
O=$LAYR_ROOT/projects/radar-overlays/lines
cd "$O/main"
echo initial > private-path
l overlay create private --mode follow -q
l overlay move private private-path >/dev/null
l save -qm private
l line new -q other main
JSON=$(l status --json)
ok "unchanged overlay files create no radar overlap" python3 -c 'import json,sys; assert not json.loads(sys.argv[1])["overlaps"]' "$JSON"
echo ours-private > private-path
l save -qam ours-private
cd "$O/other"; echo theirs-private > private-path; l save -qam theirs-private
cd "$O/main"
JSON=$(l status --json)
ok "readable overlay changes have a radar overlap" python3 -c 'import json,sys; assert any(x["path"]=="private-path" and x["line"]=="other" for x in json.loads(sys.argv[1])["overlaps"])' "$JSON"
# A batch also inherits follow overlays with ordinary singular stack records.
l line new inherited main --count 2 -q
eq "a batch line inherits an overlay" ours-private "$(cat "$O/inherited-2/private-path")"

# Service access checks also apply to radar. A denied reader receives no path list.
cd "$T"
chmod 755 "$T" "$LAYR_ROOT"
id layr-radar-reader >/dev/null 2>&1 || useradd -m layr-radar-reader
cp "$LAYR" "$T/layr"; chmod 755 "$T/layr"
export LAYR_SOCKET=$T/radar.sock
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 60 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
as_reader() { runuser -u layr-radar-reader -- env LAYR_SOCKET="$LAYR_SOCKET" LAYR_PROJECT=cloud-merge LAYR_LINE=main "$T/layr" "$@"; }
no "a user without project read access cannot query radar" as_reader status --json
l --project cloud-merge access grant layr-radar-reader reader
ok "a project reader can query radar" as_reader status --json
# Derive commands use the caller's identity and a clean environment in the service.
l --project cloud-merge line new -q caller-derive "$BASE" --owner layr-radar-reader
l --project cloud-merge config derive.command.z 'test -z "$SECRET_OF_CALLER" && test ! -r /etc/shadow && id -u > middle'
ok "derive runs as the service caller without caller secrets" runuser -u layr-radar-reader -- env SECRET_OF_CALLER=private-test-value LAYR_SOCKET="$LAYR_SOCKET" LAYR_PROJECT=cloud-merge LAYR_LINE=caller-derive "$T/layr" pick -q "$PICK"
eq "derive output names the caller uid" "$(id -u layr-radar-reader)" "$(cat "$P/caller-derive/final")"
l --project radar-overlays access grant layr-radar-reader reader
l --project radar-overlays line new -q public main --owner layr-radar-reader
# The same path in a public line must not disclose private changes in another line.
echo public-change > "$O/public/private-path"; chown layr-radar-reader "$O/public/private-path"
JSON=$(runuser -u layr-radar-reader -- env LAYR_SOCKET="$LAYR_SOCKET" LAYR_PROJECT=radar-overlays LAYR_LINE=public "$T/layr" status --json)
ok "an unreadable overlay path creates no overlap" python3 -c 'import json,sys; assert not any(x["path"]=="private-path" for x in json.loads(sys.argv[1])["overlaps"])' "$JSON"
l --project cloud-merge access deny layr-radar-reader read
no "a deny rule also blocks radar" as_reader status --json
kill "$DPID"; wait "$DPID" 2>/dev/null || true
unset LAYR_SOCKET
trap cleanup EXIT

finish
