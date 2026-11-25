#!/bin/bash
# ci: macos overlayfs
# Grep context groups and diagnostics for options formerly accepted without an effect.
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src"
printf '%s\n' before needle needle adjacent gap offscreen nearby needle > "$T/src/a.txt"
printf tail >> "$T/src/a.txt"
printf '%s\n' before needle after > "$T/src/b.txt"
l init p --no-objects --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"

want='a.txt-1-before
a.txt:2:needle
a.txt:3:needle
a.txt-4-adjacent
--
a.txt-7-nearby
a.txt:8:needle
a.txt-9-tail'
eq "context merges overlapping ranges and separates disjoint groups" "$want" "$(l grep -nC1 needle -- a.txt)"
eq "zero context keeps the usual match-only output" "$(l grep -n needle -- a.txt)" "$(l grep -n -C 0 needle -- a.txt)"
want_rev=$(printf '%s\n' "$want" | sed '/^--$/!s/^/head:/')
eq "revision context includes the revision in file names" "$want_rev" "$(l grep -n --context=1 needle head a.txt)"
eq "context continues past the last selected match" $'a.txt-1-before\na.txt:2:needle\na.txt-3-needle' \
    "$(l grep -n -C 1 -m 1 needle -- a.txt)"
eq "context can omit file names" $'1-before\n2:needle\n3-after' "$(l grep -nhC1 needle -- b.txt)"
eq "multiple files have separate context groups" "$want"$'\n--\nb.txt-1-before\nb.txt:2:needle\nb.txt-3-after' \
    "$(l grep -nC1 needle)"
printf 'b.txt\0001\000before\nb.txt\0002\000needle\nb.txt\0003\000after\n' > "$T/want"
l grep -znC1 needle -- b.txt > "$T/actual"
ok "null field separators apply to context and matches" cmp "$T/want" "$T/actual"
eq "count mode reports matches only" a.txt:3 "$(l grep -cC1 needle -- a.txt)"
l grep -qC1 needle -- a.txt > "$T/actual"; rc=$?
eq "quiet mode succeeds with a match" 0 "$rc"
no "quiet mode prints no context" test -s "$T/actual"
l grep -C1 missing -- a.txt > "$T/actual"; rc=$?
eq "context keeps the no-match exit status" 1 "$rc"
no "context prints nothing without matches" test -s "$T/actual"
has "negative context has a clear diagnostic" "grep --context needs a nonnegative integer" "$(l grep -C -1 needle 2>&1)"

reject() {
    local command=$1 option=$2 out rc
    shift 2
    out=$(l "$command" "$@" 2>&1); rc=$?
    ok "$command $option fails" test "$rc" -ne 0
    has "$command identifies $option" "unknown option: $option" "$out"
}
reject line --contains list --contains missing
reject line --format list --format=ignored
for option in -A --after-context -B --before-context; do
    reject grep "$option" "$option" 1 needle
done
for option in -a --all --fork-point; do
    reject merge-base "$option" "$option" head head
done
ok "merge-base still checks ancestry" l merge-base --is-ancestor head head
finish
