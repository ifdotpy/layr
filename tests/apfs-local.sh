#!/bin/bash
# Local APFS commands, with the default main protection and the real caller identity.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
printf 'read only\n' > "$T/src/readonly.txt"
chmod 444 "$T/src/readonly.txt"
l init local --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/local/lines
cd "$P/main" || exit 1
eq "import preserves a read-only file" "read only" "$(cat readonly.txt)"
no "main remains protected" l commit --allow-empty -qm forbidden
l branch -q topic
cd "$P/topic" || exit 1
printf 'node_modules/\n' >> .gitignore
printf 'node_modules regenerable\n' > .layrattributes
l add -A && l commit -qm attributes
mkdir -p node_modules/pkg
printf 'cache\n' > node_modules/pkg/data
ok "save converts a regenerable folder" l save -q --layers
has "the folder is a layer" "node_modules" "$(l layer list)"
no "layer data is absent from history" l show HEAD:node_modules/pkg/data
l branch -q warm 'HEAD@{0}'
eq "new line receives the saved layer" "cache" "$(cat "$P/warm/node_modules/pkg/data")"
printf 'private\n' > "$P/warm/node_modules/pkg/data"
eq "the source layer is separate" "cache" "$(cat node_modules/pkg/data)"
printf 'topic\n' > result.txt
l add result.txt && l commit -qm result
head=$(l rev-parse HEAD)
cd "$P/main" || exit 1
no "protected merge requires the expected state" l merge topic
ok "protected merge accepts the expected state" l merge topic --expect "$head"
eq "main receives the file" "topic" "$(cat result.txt)"
ok "state signatures and snapshots pass fsck" l fsck
l branch -q -D warm
ok "gc can remove obsolete APFS trees" l gc -q
ok "states remain valid after gc" l fsck
finish
