#!/bin/bash
# ci: macos overlayfs
# Optional status sections leave classification unchanged; disabling radar never enters it.
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src"
printf 'base\n' > "$T/src/shared.txt"
printf 'staged\n' > "$T/src/staged.txt"
printf '*.log\n' > "$T/src/.gitignore"
l init status-options --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/status-options/lines
cd "$P/main"
l line new -q other >/dev/null || exit 1
cd "$P/other"
printf 'other\n' > shared.txt
l save -qam other >/dev/null || exit 1
cd "$P/main"
l line new -q mine >/dev/null || exit 1
cd "$P/mine"
printf 'mine\n' > shared.txt
printf 'staged change\n' > staged.txt
l add staged.txt >/dev/null || exit 1
printf 'untracked\n' > new.txt
printf 'ignored\n' > debug.log

ok "no-overlaps JSON succeeds" l status --json --no-overlaps
ok "no-ignored JSON succeeds" l status --json --no-ignored
# Stop here on an older binary: subsequent checks require valid JSON from both switches.
if [ "$FAIL" -ne 0 ]; then finish; exit 1; fi

LAYR_TRACE=1 l status --json --no-overlaps > "$T/no-overlaps.json" 2> "$T/no-overlaps.trace"
no "disabled radar is never entered" grep -E 'status overlaps|overlap live|overlap compare' "$T/no-overlaps.trace"
no "disabled radar creates no compare cache" test -d "$LAYR_ROOT/projects/status-options/meta/overlap-compares"
no "disabled radar creates no live cache" test -d "$LAYR_ROOT/projects/status-options/meta/overlap-live"
l status --json > "$T/default.json"
l status --json --no-ignored > "$T/no-ignored.json"
l status --json --no-overlaps --no-ignored > "$T/neither.json"
ok "JSON switches change only their requested sections" python3 - "$T" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
def load(name):
    return json.loads((root / (name + '.json')).read_text())
default = load('default')
assert set(default) == {'line', 'head', 'staged', 'unstaged', 'untracked', 'ignored', 'conflicts', 'overlaps'}
assert default['ignored'] == ['debug.log']
assert default['untracked'] == ['new.txt']
assert [c['path'] for c in default['staged']] == ['staged.txt']
assert [c['path'] for c in default['unstaged']] == ['shared.txt']
assert [(o['path'], o['line']) for o in default['overlaps']] == [('shared.txt', 'other')]
assert load('no-overlaps') == {k: v for k, v in default.items() if k != 'overlaps'}
without_ignored = {k: v for k, v in default.items() if k != 'ignored'}
assert load('no-ignored') == without_ignored
assert load('neither') == {k: v for k, v in without_ignored.items() if k != 'overlaps'}
PY
has "default long output includes radar" 'Overlaps with other live lines:' "$(l status)"
no "no-overlaps long output omits radar" sh -c '"$LAYR" status --no-overlaps | grep -q "Overlaps with"'
eq "short output is unchanged" "$(l status -s)" "$(l status -s --no-overlaps --no-ignored)"
eq "porcelain output is unchanged" "$(l status --porcelain)" "$(l status --porcelain --no-overlaps --no-ignored)"
has "ignored short output is available" '!! debug.log' "$(l status -s --ignored)"
eq "no-ignored overrides ignored regardless of order" "$(l status -s)" "$(l status -s --no-ignored --ignored)"
eq "no-ignored overrides ignored in reverse order" "$(l status -s)" "$(l status -s --ignored --no-ignored)"
eq "no-ignored long output matches the default" "$(l status --no-overlaps)" "$(l status --no-overlaps --ignored --no-ignored)"
eq "path filtering is unchanged" "$(l status -s -- shared.txt)" "$(l status -s --no-overlaps --no-ignored -- shared.txt)"

# Open conflicts remain visible with either switch.
l save -qam mine >/dev/null || exit 1
no "merge leaves an open content conflict" l merge other
l status --json > "$T/conflict-default.json"
l status --json --no-overlaps --no-ignored > "$T/conflict-options.json"
ok "conflict classification is unchanged" python3 - "$T" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
default = json.loads((root / 'conflict-default.json').read_text())
options = json.loads((root / 'conflict-options.json').read_text())
assert default['conflicts']
assert options == {k: v for k, v in default.items() if k not in ('ignored', 'overlaps')}
PY

finish
