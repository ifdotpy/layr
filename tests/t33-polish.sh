#!/bin/bash
# Human behavior reports, omitted-state summaries, and literal command configuration.
. "$(dirname "$0")/lib.sh"
set -e
mkdir -p "$T/src"
echo 0 > "$T/src/version"
printf 'out/\n' > "$T/src/.gitignore"
cat > "$T/src/layr.toml" <<'TOML'
[probe.metrics]
run = ['python3', 'probe.py', 'number']
outputs = ['out/number.json']
kind = 'number'
[probe.data]
run = ['python3', 'probe.py', 'json']
outputs = ['out/data.json']
kind = 'json'
[probe.files]
run = ['python3', 'probe.py', 'files']
outputs = ['out/files']
[probe.missing]
run = ['true']
outputs = ['out/missing']
TOML
cat > "$T/src/probe.py" <<'PY'
import json, pathlib, sys
v = int(pathlib.Path('version').read_text())
p = pathlib.Path('out'); p.mkdir(exist_ok=True)
if sys.argv[1] == 'number':
    values = [dict(time=100, down=100, quiet=100, zero=0),
              dict(time=126, down=74, quiet=104, zero=1),
              dict(time=104, down=96, quiet=103, zero=0)][v]
    (p/'number.json').write_text(json.dumps(values))
elif sys.argv[1] == 'json':
    values = {'a/null': None, **{f'z{i:02}': v for i in range(23)}}
    values['c/added' if v else 'b/removed'] = None
    (p/'data.json').write_text(json.dumps(values))
else:
    (p/'files').mkdir(exist_ok=True)
    (p/'files'/'text').write_text('new\nextra\n' if v else 'old\n')
    (p/'files'/'binary').write_bytes(b'\x00newer' if v else b'\x00old')
PY
l init polish --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/polish/lines/main"
BASE=$(l rev-parse head)
for name in metrics data files; do l probe run "$BASE" --probe "$name" >/dev/null; done
l line new topic -q
cd ../topic
echo 1 > version
l save -qam changed
HEAD=$(l rev-parse head)
for name in metrics data files; do l probe run "$HEAD" --probe "$name" >/dev/null; done
HUMAN=$(l probe diff "$BASE" "$HEAD")
has "metric values omit decimal zeros and show a signed rounded percent" 'out/number.json   time 100 → 126 (+26%)' "$HUMAN"
has "negative percentages have a minus sign" 'down 100 → 74 (-26%)' "$HUMAN"
has "zero baselines omit an undefined percentage" 'zero 0 → 1' "$HUMAN"
no "metrics within noise are absent" grep -q 'quiet 100' <<< "$HUMAN"
has "text files show line counts" 'out/files/text   +2 −1 lines' "$HUMAN"
has "binary files show byte counts" 'out/files/binary   binary, 4 → 6' "$HUMAN"
has "removed null differs from absent" 'out/data.json   /b~1removed: null → (absent)' "$HUMAN"
has "added null differs from absent" 'out/data.json   /c~1added: (absent) → null' "$HUMAN"
has "JSON changes stop after twenty rows" '    … 5 more' "$HUMAN"
eq "JSON has twenty visible rows" 20 "$(grep -c 'out/data.json   ' <<< "$HUMAN")"
has "missing probe result" '  missing: not run' "$HUMAN"
has "unchanged probe result" '  files: unchanged' "$(l probe diff "$HEAD" "$HEAD")"
JSON=$(l probe diff "$BASE" "$HEAD" --json)
ok "machine output retains all structural rows and its schema" python3 -c 'import json,sys; d=json.loads(sys.argv[1]); assert set(d)=={"base","head","base_source","head_source","probes"}; p=next(p for p in d["probes"] if p["probe"]=="data"); assert len(p["changes"][0]["changes"])==25; assert set(p)=={"probe","status","base","head","exit_changed","changes"}' "$JSON"
CHANGE=$(l change list --json | python3 -c 'import json,sys; print(next(r["change_id"] for r in json.load(sys.stdin) if r["message"]=="changed"))')
SHOW=$(l change show "$CHANGE")
has "change show uses the human behavior report" "$HUMAN" "$SHOW"
echo 2 > version
l save -qam noise
QUIET=$(l rev-parse head)
l probe run "$QUIET" --probe metrics >/dev/null
NOISE=$(l probe diff "$BASE" "$QUIET")
has "all numeric changes within noise explain the threshold" '    no change beyond noise (5%)' "$NOISE"
has "noise preserves unchanged status" '  metrics: unchanged' "$NOISE"
# Command text stays exact even when it is valid JSON.
for key in check.unit derive.command.unit merge.driver.unit merge.verify.unit; do
    for value in 'false' 'null' '123' '["true"]' 'true'; do
        l config "$key" "$value"
        eq "$key keeps $value as command text" "$value" "$(l config --get "$key")"
    done
done
l config check.exact '  "true"  '
eq "command quotes and spaces remain exact" '  "true"  ' "$(l config --get check.exact)"
l config remote.archive.insecure true
ok "command settings are strings and the remote flag remains boolean" python3 - "$LAYR_ROOT/projects/polish/meta/db/layr.sqlite" <<'PY'
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
values = {k: json.loads(v) for k, v in c.execute('select key,value from config where value is not null')}
for key in ['check.unit','derive.command.unit','merge.driver.unit','merge.verify.unit']:
    assert values[key] == 'true', (key, values[key])
assert values['remote.archive.insecure'] is True
PY
cd ../main
l protect main --check unit
ok "a check command literally true executes during merge" l merge topic --expect "$QUIET" -q
has "the passing true check is recorded" 'true' "$(l attestations head --kind check)"
# Omitted Auto states are not ancestry dependencies of a line head.
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
for n in 1 2 3; do echo "$n" > unsaved; l save -q; done
PUSH=$(l push archive)
no "a project without overlays has no private-file notice" grep -q 'private overlay files' <<< "$PUSH"
ok "Auto states have one summary with the option hint" grep -Eq '^omitted: [0-9]+ auto states \(use --auto\)$' <<< "$PUSH"
PUSH_JSON=$(l push archive --json)
ok "machine output keeps the omitted state IDs" python3 -c 'import json,sys; d=json.loads(sys.argv[1]); assert any("auto" in s and len(s.split()[0])==36 for s in d["omitted"])' "$PUSH_JSON"
# An empty overlay does not cause a notice.
l overlay create private -q
l overlay use private -q
echo shared > shared
l add shared
l save -qm empty-overlay
EMPTY=$(l push archive)
no "an empty overlay has no private-file notice" grep -q 'private overlay files' <<< "$EMPTY"
echo confidential > private.txt
l overlay move private private.txt
l add private.txt
l save -qm private-only
echo next > shared
l save -qam with-private
PRIVATE=$(l push archive)
# Private overlays travel encrypted (docs/overlay-sync-design.md), so nothing is left out.
no "pushed private overlay files are not left out" grep -q 'private overlay files' <<< "$PRIVATE"
ok "the store holds the encrypted overlay" test -d "$T/archive/overlays"
no "the store holds no plaintext of the private file" grep -rq confidential "$T/archive"
REPEAT=$(l push archive)
no "a repeat push with no states has no private-file notice" grep -q 'private overlay files' <<< "$REPEAT"
finish
