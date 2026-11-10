#!/bin/bash
# ci: macos overlayfs
# A release ref can be created only if it does not exist.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
base=$(l rev-parse head)
ok 'create an absent ref' l ref set release/v1.0.0 head --expect none
no 'the same creation cannot overwrite the ref' l ref set release/v1.0.0 head --expect none
eq 'the original ref stays intact' "$base" "$(l rev-parse release/v1.0.0)"
(l ref set release/v2.0.0 head --expect none > /dev/null 2>&1; echo $? > "$T/one") &
(l ref set release/v2.0.0 head --expect none > /dev/null 2>&1; echo $? > "$T/two") &
wait
eq 'concurrent creates have one winner' '0 128' "$(sort -n "$T/one" "$T/two" | xargs)"
finish
