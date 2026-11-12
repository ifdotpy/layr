#!/bin/bash
# ci: macos
# A line delete takes the derived data of the line with it (docs/ownership.md, "Derived data of a state"): its saved
# status goes at the end of the command, with no `gc`.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l line new x main >/dev/null 2>&1
id=$(l line list --json 2>/dev/null | python3 -c 'import json,sys; print([l["id"] for l in json.load(sys.stdin) if l["name"]=="x"][0])')
(cd "$LAYR_ROOT/projects/p/lines/x" && echo change > c.txt && l status >/dev/null 2>&1)
status_file=$LAYR_ROOT/projects/p/meta/status/$id.cache
ok "a status saves the line's status" test -f "$status_file"
l line delete -D --discard x >/dev/null 2>&1
no "the line delete removes it" test -e "$status_file"
finish
