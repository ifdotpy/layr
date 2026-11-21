#!/bin/bash
# ci: macos overlayfs
# `layr status` pairs a staged move as a rename, and `--no-renames` shows it as a delete and an add
# in every output format, as `git status --no-renames` does. Unstaged changes are the same either way.
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src"
printf 'one\ntwo\nthree\n' > "$T/src/z.txt"
printf 'keep\n' > "$T/src/keep.txt"
l init no-renames --from "$T/src" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/no-renames/lines/main" || exit 1
l line new -q topic >/dev/null || exit 1
cd ../topic || exit 1
l mv z.txt a.txt >/dev/null || exit 1
printf 'edit\n' >> keep.txt

eq "short pairs the move by default" "$(printf 'R  z.txt -> a.txt\n M keep.txt')" "$(l status -s)"
eq "short with --no-renames splits the move" "$(printf 'A  a.txt\n M keep.txt\nD  z.txt')" "$(l status -s --no-renames)"
eq "porcelain with --no-renames splits the move" "$(printf 'A  a.txt\n M keep.txt\nD  z.txt')" "$(l status --porcelain --no-renames)"
eq "porcelain -z pairs the move by default" "R  a.txt z.txt  M keep.txt " "$(l status --porcelain -z | tr '\0' ' ')"
eq "porcelain -z with --no-renames splits the move" "A  a.txt  M keep.txt D  z.txt " "$(l status --porcelain -z --no-renames | tr '\0' ' ')"
# Bare -z selects the NUL-terminated short format, byte for byte as git prints it.
l status -z > "$T/z.out"; l status -z --no-renames > "$T/z-split.out"
printf 'R  a.txt\0z.txt\0 M keep.txt\0' > "$T/z.want"
printf 'A  a.txt\0 M keep.txt\0D  z.txt\0' > "$T/z-split.want"
ok "bare -z prints the paired move as porcelain v1" cmp "$T/z.want" "$T/z.out"
ok "bare -z --no-renames prints the split move as porcelain v1" cmp "$T/z-split.want" "$T/z-split.out"
LAYR_TRACE=1 l status -z > /dev/null 2> "$T/z.trace"
ok "bare -z runs with a trace" test -s "$T/z.trace"
no "bare -z skips the overlap check" grep -q "status overlaps" "$T/z.trace"
has "long pairs the move by default" "renamed:    z.txt -> a.txt" "$(l status)"
long=$(l status --no-renames)
has "long with --no-renames lists the add" "new file:   a.txt" "$long"
has "long with --no-renames lists the delete" "deleted:    z.txt" "$long"
no "long with --no-renames has no rename" sh -c '"$LAYR" status --no-renames | grep -q renamed:'
eq "a path filter keeps one end of the split move" "$(printf 'D  z.txt')" "$(l status -s --no-renames -- z.txt)"
eq "a path filter keeps the other end too" "$(printf 'A  a.txt')" "$(l status -s --no-renames -- a.txt)"

l status --json > "$T/default.json"
l status --json --no-renames > "$T/split.json"
ok "JSON splits the staged move and keeps the other sections" python3 - "$T" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
default = json.loads((root / 'default.json').read_text())
split = json.loads((root / 'split.json').read_text())
rows = lambda d: [(c['path'], c['old_path']) for c in d['staged']]
assert rows(default) == [('a.txt', 'z.txt')], rows(default)
assert rows(split) == [('a.txt', None), ('z.txt', None)], rows(split)
for key in default:
    if key != 'staged':
        assert default[key] == split[key], key
assert [c['path'] for c in split['unstaged']] == ['keep.txt']
PY

# The flag does not change what a save records.
l save -qam move >/dev/null || exit 1
eq "the saved state still records the move" "R100	z.txt	a.txt" "$(l show --name-status --format= head | grep ^R)"
eq "a clean line shows nothing with the flag" "" "$(l status -s --no-renames)"

# A move out of a quiet overlay: the split comes before the quiet files are left out, so the quiet source
# delete does not show (a split after the filter would show it).
printf 'k1\n' > .env
l overlay create keys -q || exit 1
l config overlay.keys.quiet true >/dev/null
l overlay move keys .env >/dev/null || exit 1
l save -q -m keys || exit 1
l mv .env visible.txt || exit 1
eq "a quiet source stays out of the split" "A  visible.txt" "$(l status -s --no-renames)"

finish
