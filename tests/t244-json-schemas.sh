#!/bin/bash
# ci: macos
# The --json output of status, line list, queue list, change list, change show and attestations matches its schema in
# schemas/ (docs/json.md), and `layr schema <name>` prints the same file. The check is strict: an output field that the
# schema does not describe fails, so a change of the output must change the schema too.
. "$(dirname "$0")/lib.sh"
SRC=$(cd "$(dirname "$0")/.." && pwd)
CHECK="python3 -I $SRC/tests/json-schema.py"
OUT=$T/out
mkdir -p "$OUT"

# check <description> <schema>[#<definition>] <command...>: the command's output validates against
# schemas/<schema>.json, or against its definition $defs/<definition>. CHECK_EXIT names another exit status that
# the command must end with (a save with nothing to save ends with 1).
check() {
    local d=$1 schema=${2%%#*} def=() file errors rc
    [ "$2" = "$schema" ] || def=(--def "${2#*#}")
    shift 2
    file=$OUT/$(printf '%s' "$d" | tr -c 'a-z0-9' -).json
    "$@" > "$file" 2>"$file.err"
    rc=$?
    if [ "$rc" != "${CHECK_EXIT:-0}" ]; then
        FAIL=$((FAIL + 1)); echo "FAIL $d (exit status $rc)"; sed 's/^/  /' "$file.err" | head -5; return
    fi
    if errors=$($CHECK check "$SRC/schemas/$schema.json" "$file" --strict ${def[@]+"${def[@]}"} 2>&1); then
        PASS=$((PASS + 1)); echo "PASS $d"
    else
        FAIL=$((FAIL + 1)); echo "FAIL $d"; echo "$errors" | sed 's/^/  /' | head -10
    fi
}

for schema in "$SRC"/schemas/*.json; do
    name=$(basename "$schema" .json)
    ok "schema $name is well formed and describes each field" $CHECK lint "$schema"
    eq "layr schema $name prints schemas/$name.json" "$(cat "$schema")" "$(l schema "$name")"
done
eq 'layr schema lists the schemas' "$(cd "$SRC/schemas" && ls *.json | sed 's/\.json$//')" "$(l schema)"
no 'an unknown schema name fails' l schema nothing

# The fixture: a change with two versions, checks, an approval, an origin and a comment; overlaps; a rename; a
# conflict; queue entries that are done, failed and pending.
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
l line new -q topic
cd ../topic
echo one > one.txt; l add one.txt
check 'save -m --json' save l save -m one --json
CHECK_EXIT=1 check 'save -m --json with nothing to save' save l save -m nothing --json
l try -q head -- true 2>/dev/null
l try -q head -- sh -c 'exit 3' 2>/dev/null
l approve -q head
printf 'payload' > "$T/payload"
l attest origin head --payload "$T/payload" >/dev/null
echo two >> one.txt; l save -qam 'one, amended' --amend
C=$(l change list --json | python3 -c 'import json,sys; print(next(r["change_id"] for r in json.load(sys.stdin) if r["message"].startswith("one")))')
NOTE=$(l comment "$C" 'a note' --file one.txt --line 1-2 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
l comment "$C" 'a whole-change note' --version v1 >/dev/null
l comment reply "$NOTE" 'an answer' >/dev/null
l comment resolve "$NOTE" >/dev/null
l react "$NOTE" +1 >/dev/null
l react "$C" heart >/dev/null
l review request "$C" "user:$(id -u)" >/dev/null
l change edit "$C" --title 'feat: one' --description 'The first file.' >/dev/null
l line new -q other main
(cd ../other && echo other >> notes.txt && l save -qam other)
OTHER=$(l change list --json | python3 -c 'import json,sys; print(next(r["change_id"] for r in json.load(sys.stdin) if r["message"]=="other"))')
l change close "$OTHER" -m 'not needed' >/dev/null
echo topic >> notes.txt
echo new > staged.txt; l add staged.txt
l mv one.txt renamed.txt
echo loose > loose.txt
check 'status --json' status l status --json
check 'save --json (an automatic state)' save l save --json
eq 'the fixture has a staged rename' renamed.txt "$(l status --json | python3 -c 'import json,sys; print(next(c["path"] for c in json.load(sys.stdin)["staged"] if c["old_path"]))')"
eq 'the fixture has an overlap' other "$(l status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["overlaps"][0]["line"])')"
check 'status --json --no-overlaps --no-ignored' status l status --json --no-overlaps --no-ignored
check 'status --json of a path' status l status --json notes.txt
l save -qam 'topic notes' 2>/dev/null
l line new -q c1 main
l line new -q c2 main
(cd ../c1 && echo c1 > notes.txt && l save -qam c1)
(cd ../c2 && echo c2 > notes.txt && l save -qam c2)
cd ../c1
l merge c2 >/dev/null 2>&1
check 'status --json with conflicts' status l status --json
eq 'the fixture has a conflict' notes.txt "$(l status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["conflicts"][0]["path"])')"
cd ../topic
check 'line list --json' line-list l line list --json
check 'line list --json with a pattern' line-list l line list --json 'c*'
check 'queue add --json' 'queue-list#entry' l queue add topic --into main --json
l queue add c2 --into other -m 'fix: c2' >/dev/null
l line new -q q1 main
l line new -q q2 main
(cd ../q1 && echo q1 > notes.txt && l save -qam q1)
(cd ../q2 && echo q2 > notes.txt && l save -qam q2)
l queue add q2 --into q1 >/dev/null
(cd ../main && l queue run --into main >/dev/null 2>&1)
cd ../q1
CHECK_EXIT=1 check 'queue run --json with a failed entry' queue-list l queue run --into q1 --json
cd ../topic
check 'queue list --json' queue-list l queue list --json
eq 'the fixture has done, pending and failed entries' 'done failed pending' "$(l queue list --json | python3 -c 'import json,sys; print(" ".join(sorted({e["status"] for e in json.load(sys.stdin)})))')"
check 'queue list --into --json' queue-list l queue list --into q1 --json
check 'change list --json' change-list l change list --json
check 'change list --all --json' change-list l change list --all --json
check 'change list --closed --json' change-list l change list --closed --json
eq 'the fixture has open, closed and merged changes' 'closed merged open' "$(l change list --all --json | python3 -c 'import json,sys; print(" ".join(sorted({r["status"] for r in json.load(sys.stdin)})))')"
check 'change show --json of a closed change' change-show l change show "$OTHER" --json
eq 'a closed change names its reason' 'not needed' "$(l change show "$OTHER" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["closed"]["reason"])')"
check 'change show --json' change-show l change show "$C" --json
l change show "$C" --json > "$T/review.json"
eq 'the fixture change has a title, a thread with a reply, reactions and a reviewer' 'feat: one|1 1 2|True|1 1|outdated' "$(python3 - "$T/review.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
t = c["threads"][0]
print(f"{c['title']}|{len(t['replies'])} {t['comment']['reactions']['+1']} {t['line_end']}|{t['resolved']}|"
      f"{c['reactions']['heart']} {len(c['reviewers'])}|{c['reviewers'][0]['status']}")
PY
)"
eq 'the fixture change has two versions, checks, comments and an operation' '2 2 3 amend' "$(l change show "$C" --json | python3 -c '
import json,sys
c = json.load(sys.stdin)
print(len(c["versions"]), len(c["versions"][0]["checks"]), len(c["comments"]), c["versions"][1]["operation"]["kind"])')"
MERGE=$(l change list --json | python3 -c 'import json,sys; print(next(r["change_id"] for r in json.load(sys.stdin) if r["message"].startswith("Merge ")))')
check 'change show --json of a merge with conflicts' change-show l change show "$MERGE" --json
check 'attestations --json' attestations l attestations "changes/$C/v1" --json
check 'attestations --kind --json' attestations l attestations "changes/$C/v1" --kind check --json
eq 'the fixture state has four attestations' 4 "$(l attestations "changes/$C/v1" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"

# The checker fails on a known bad output: a missing field, a wrong type, an undescribed field, a bad schema.
python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); del s["head"]; json.dump(s, open(sys.argv[2], "w"))' \
    "$OUT/status---json.json" "$T/bad-missing.json"
no 'the checker refuses an output without a required field' $CHECK check "$SRC/schemas/status.json" "$T/bad-missing.json"
python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); s["staged"][0]["status"]="X"; json.dump(s, open(sys.argv[2], "w"))' \
    "$OUT/status---json.json" "$T/bad-value.json"
no 'the checker refuses a value outside the schema' $CHECK check "$SRC/schemas/status.json" "$T/bad-value.json"
python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); s[0]["undescribed"]=1; json.dump(s, open(sys.argv[2], "w"))' \
    "$OUT/line-list---json.json" "$T/bad-extra.json"
no 'the strict checker refuses an undescribed field' $CHECK check "$SRC/schemas/line-list.json" "$T/bad-extra.json" --strict
ok 'a consumer may get more fields (the schema allows them)' $CHECK check "$SRC/schemas/line-list.json" "$T/bad-extra.json"
python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); del s["$defs"]["line"]["properties"]["name"]["description"]; json.dump(s, open(sys.argv[2], "w"))' \
    "$SRC/schemas/line-list.json" "$T/bad-schema.json"
no 'lint refuses a field without a description' $CHECK lint "$T/bad-schema.json"
finish
