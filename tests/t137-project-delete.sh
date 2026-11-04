#!/bin/bash
# ci: macos overlayfs
# `layr project delete <project>` removes a project without lines through the trash of its
# volume. A project with lines, an overlay name and an unknown project are refused.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init gone --from "$T/src" -q >/dev/null
l init kept --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/gone/lines/main" || exit 1
l line new -q side
cd "$T" || exit 1

out=$(l project delete gone 2>&1)
no "a project with lines is refused" test $? -eq 0
has "the refusal names the lines" "has 2 line(s): main, side" "$out"
ok "the refused project stays" test -d "$LAYR_ROOT/projects/gone/lines/main"
has "an overlay name is refused" "is an overlay" "$(l project delete gone%cache 2>&1)"
has "an unknown project is refused" "not found" "$(l project delete nothing 2>&1)"

l --project gone line delete side >/dev/null
l --project gone line delete main >/dev/null
has "the project has no lines" "gone	0 line(s)" "$(l projects)"
eq "delete prints the project" "deleted project gone" "$(l project delete gone 2>&1)"
ok "the project folder is gone" test ! -e "$LAYR_ROOT/projects/gone"
for _ in $(seq 1 100); do [ -z "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)" ] && break; sleep 0.2; done
eq "the trash empties in the background" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
no "projects does not list it" grep -q "^gone" <<<"$(l projects)"
has "the other project stays" "kept	1 line(s)" "$(l projects)"
has "a second delete finds nothing" "not found" "$(l project delete gone 2>&1)"
ok "the name can be used again" l init gone --from "$T/src" -q
eq "the new project starts clean" "main" "$(l --project gone line list | awk '{print $NF}' | tr '\n' ' ' | sed 's/ $//')"
finish
