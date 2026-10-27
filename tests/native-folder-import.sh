#!/bin/bash
# The local locator must not enter imported history or regenerated caches.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
mkdir -p "$T/src/.layr"
printf 'private locator\n' > "$T/src/.layr/link.json"
printf '.layr regenerable\n' > "$T/src/.layrattributes"
printf '.layr/\n' > "$T/src/.gitignore"
l init linked-source --from "$T/src" --include-ignored -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/linked-source/lines/main" || exit 1
no "import excludes the workspace locator" test -e .layr
no "history excludes the workspace locator" l show HEAD:.layr/link.json
eq "import preserves the input locator" "private locator" "$(cat "$T/src/.layr/link.json")"
ok "imported history passes verification" l fsck
finish
